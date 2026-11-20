//! Python 3.15.1 `sys.stdout` and `sys.stderr`, and argv decoding with surrogateescape.
//!
//! Each stream copies CPython's two layers so the same `write(2)` calls happen with the same
//! sizes as in Python, which is what decides the outcome on a closed pipe:
//! - `TextIOWrapper`: text is encoded and added to the pending bytes; once they reach the
//!   8192-byte chunk size they are handed down in one piece (probed: writes of 8000 then 1000
//!   bytes reach the buffer as one write of 9000). With `line_buffering` (stdout on a TTY,
//!   stderr always), a write that holds `\n` or `\r` hands down and flushes.
//! - `BufferedWriter`: bytes that fit are copied into the buffer of
//!   `max(min(st_blksize, 8 MiB), 128 KiB)` bytes when they fit and are shorter than the buffer.
//!   Else the buffer is flushed first, then the bytes go to `write(2)` directly while a buffer's
//!   worth or more is left; the rest is copied in (probed: one write of exactly 131072 bytes to a
//!   closed pipe fails at once, one of 131071 is buffered).
//!   A failed flush keeps the bytes not yet written, so the flush at exit tries them again.
//!
//! The closed-pipe rule (Python 3.15.1, macOS, `| head -c 10`):
//! - A `write(2)` that fails during the run raises `BrokenPipeError: [Errno 32] Broken pipe`.
//!   The CLI catches it as OSError: `error: [Errno 32] Broken pipe`, return 1.
//! - At exit the flush runs. If bytes are still buffered and the write fails, Python prints
//!   `Exception ignored while flushing sys.stdout:` and `BrokenPipeError: [Errno 32] Broken pipe`
//!   to stderr and the exit status is 120, whatever status the run returned.
//! - So: output that ends in the buffer and is written only at exit gives 0 when the reader still
//!   reads, else 120 with 2 stderr lines; a direct write larger than the buffer that fails during
//!   the run leaves nothing buffered and gives 1 with 1 line; a buffer flush that fails during the
//!   run leaves the bytes buffered and gives 120 with 3 lines.
//!
//! Which `write(2)` fails depends on when the reader exits and on the kernel pipe size, so it is
//! timing-dependent; this module makes the same calls as Python, not the same timing.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

use crate::error::{Base, SmError};
pub use crate::py::str::{repr, surrogate_byte};

const CHUNK_SIZE: usize = 8192;
const DEFAULT_BUFFER_SIZE: usize = 128 * 1024;
const MAX_BUFFER_SIZE: usize = 8192 * 1024;

enum Sink {
    Fd(i32),
    /// The bytes written, and the size of each `write(2)` the fd would have seen.
    #[cfg(test)]
    Mem(Vec<u8>, Vec<usize>),
}

/// How a stream encodes a surrogateescape stand-in char (a lone surrogate to Python). Python 3.15.1
/// runs in UTF-8 mode (`sys.flags.utf8_mode == 1`), so `sys.stdout.errors` is `surrogateescape`
/// and `sys.stderr.errors` is always `backslashreplace`, under any LANG or LC_ALL (probed with
/// `env -i`). Only `PYTHONIOENCODING` changes stdout's (`stdout_errors`).
#[derive(Clone, Debug, PartialEq)]
enum Errors {
    /// `print('x\udcff')` writes the raw byte 0xFF.
    SurrogateEscape,
    /// A UnicodeEncodeError (`encode_text`), nothing written.
    Strict,
    /// `?` for each stand-in.
    Replace,
    Ignore,
    /// `\udcXX`; `namereplace` gives the same for a surrogate, which has no name.
    BackslashReplace,
    /// `&#56575;`.
    XmlCharRefReplace,
    /// The three-byte form of the lone surrogate, as UTF-8 `surrogatepass` writes it.
    SurrogatePass,
    /// A name the codec registry lacks: a LookupError when a stand-in is written.
    Unknown(String),
}

impl Errors {
    fn named(name: &str) -> Errors {
        match name {
            "strict" => Errors::Strict,
            "surrogateescape" => Errors::SurrogateEscape,
            "replace" => Errors::Replace,
            "ignore" => Errors::Ignore,
            "backslashreplace" | "namereplace" => Errors::BackslashReplace,
            "xmlcharrefreplace" => Errors::XmlCharRefReplace,
            "surrogatepass" => Errors::SurrogatePass,
            other => Errors::Unknown(other.to_string()),
        }
    }
}

/// `sys.stdout.errors` for a `PYTHONIOENCODING` value (CPython `config_init_stdio_encoding`): a
/// non-empty value is `[encoding][:errors]` split at the first colon. An encoding with no handler
/// means `strict`; a handler alone replaces the UTF-8-mode default `surrogateescape`; an empty
/// value is unset. A non-UTF-8 encoding name is not modelled: stdout stays UTF-8.
fn stdout_errors(pythonioencoding: Option<&str>) -> Errors {
    let Some(v) = pythonioencoding.filter(|v| !v.is_empty()) else {
        return Errors::SurrogateEscape;
    };
    let (encoding, errors) = match v.split_once(':') {
        Some((e, r)) => (e, Some(r).filter(|r| !r.is_empty())),
        None => (v, None),
    };
    match errors.or((!encoding.is_empty()).then_some("strict")) {
        Some(name) => Errors::named(name),
        None => Errors::SurrogateEscape,
    }
}

struct Stream {
    sink: Sink,
    pending: Vec<u8>,
    buf: Vec<u8>,
    buf_size: usize,
    line_buffering: bool,
    errors: Errors,
}

/// stdout and stderr of the process, or in memory for tests.
pub struct Out {
    stdout: Stream,
    stderr: Stream,
}

impl Out {
    /// fd 1 and fd 2, set up as Python 3.15.1 sets up `sys.stdout` and `sys.stderr`, with the stdout
    /// error handler `PYTHONIOENCODING` names.
    pub fn new() -> Out {
        Out::on_fds(1, 2, std::env::var("PYTHONIOENCODING").ok().as_deref())
    }

    /// `sys.stdout` on `stdout_fd` (line-buffered only on a TTY) and `sys.stderr` on `stderr_fd`
    /// (always line-buffered).
    fn on_fds(stdout_fd: i32, stderr_fd: i32, pythonioencoding: Option<&str>) -> Out {
        Out {
            stdout: Stream::fd(
                stdout_fd,
                is_tty(stdout_fd),
                stdout_errors(pythonioencoding),
            ),
            stderr: Stream::fd(stderr_fd, true, Errors::BackslashReplace),
        }
    }

    /// In-memory streams that never fail; stdout is block-buffered as on a pipe.
    #[cfg(test)]
    pub fn capture() -> Out {
        Out {
            stdout: Stream::mem(false, Errors::SurrogateEscape),
            stderr: Stream::mem(true, Errors::BackslashReplace),
        }
    }

    /// `print(s)`: `write(s)`, then `write("\n")`.
    pub fn print(&mut self, s: &str) -> Result<(), SmError> {
        self.stdout.write_text(s)?;
        self.stdout.write_text("\n")
    }

    /// `sys.stdout.write(s)`, or `print(s, end="")`.
    pub fn write(&mut self, s: &str) -> Result<(), SmError> {
        self.stdout.write_text(s)
    }

    /// `sys.stdout.buffer.write(b)`.
    pub fn write_bytes(&mut self, b: &[u8]) -> Result<(), SmError> {
        self.stdout.flush_pending()?;
        self.stdout.buffer_write(b)
    }

    /// `sys.stdout.flush()`.
    pub fn flush(&mut self) -> Result<(), SmError> {
        self.stdout.flush()
    }

    /// `print(s, file=sys.stderr)`.
    pub fn eprint(&mut self, s: &str) -> Result<(), SmError> {
        self.stderr.write_text(s)?;
        self.stderr.write_text("\n")
    }

    /// `sys.stderr.write(s)`.
    pub fn ewrite(&mut self, s: &str) -> Result<(), SmError> {
        self.stderr.write_text(s)
    }

    /// Interpreter exit: flush stdout, then stderr. A failed stdout flush prints Python's
    /// unraisable text and makes the status 120.
    pub fn finish(&mut self, code: i32) -> i32 {
        let mut code = code;
        if let Err(e) = self.stdout.flush() {
            let _ = self.ewrite(&format!(
                "Exception ignored while flushing sys.stdout:\n{}: {}\n",
                e.class, e.msg
            ));
            code = 120;
        }
        let _ = self.stderr.flush();
        code
    }

    /// What a `capture()` Out holds: (stdout bytes written so far, stderr bytes written so far).
    #[cfg(test)]
    pub fn captured(&self) -> (String, String) {
        (self.stdout.captured(), self.stderr.captured())
    }
}

impl Default for Out {
    fn default() -> Out {
        Out::new()
    }
}

impl Stream {
    fn fd(fd: i32, line_buffering: bool, errors: Errors) -> Stream {
        Stream {
            sink: Sink::Fd(fd),
            pending: Vec::new(),
            buf: Vec::new(),
            buf_size: buffer_size(fd),
            line_buffering,
            errors,
        }
    }

    #[cfg(test)]
    fn mem(line_buffering: bool, errors: Errors) -> Stream {
        Stream {
            sink: Sink::Mem(Vec::new(), Vec::new()),
            pending: Vec::new(),
            buf: Vec::new(),
            buf_size: DEFAULT_BUFFER_SIZE,
            line_buffering,
            errors,
        }
    }

    #[cfg(test)]
    fn captured(&self) -> String {
        match &self.sink {
            Sink::Mem(v, _) => String::from_utf8_lossy(v).into_owned(),
            Sink::Fd(_) => String::new(),
        }
    }

    /// `TextIOWrapper.write`.
    fn write_text(&mut self, text: &str) -> Result<(), SmError> {
        let b = self.encode(text)?;
        let needflush = self.line_buffering && (text.contains('\n') || text.contains('\r'));
        self.pending.extend_from_slice(&b);
        if self.pending.len() >= CHUNK_SIZE || needflush {
            self.flush_pending()?;
        }
        if needflush {
            self.flush_buffer()?;
        }
        Ok(())
    }

    fn encode(&self, text: &str) -> Result<Vec<u8>, SmError> {
        if !text.chars().any(|c| surrogate_byte(c).is_some()) {
            return Ok(text.as_bytes().to_vec());
        }
        match &self.errors {
            Errors::Strict => return crate::py::path::encode_text(text),
            Errors::Unknown(name) => {
                return Err(SmError::from_python(
                    "LookupError",
                    format!("unknown error handler name {}", repr(name)),
                ));
            }
            _ => {}
        }
        let mut b = Vec::with_capacity(text.len());
        for c in text.chars() {
            let Some(x) = surrogate_byte(c) else {
                b.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                continue;
            };
            let cp = 0xDC00 + u32::from(x);
            match self.errors {
                Errors::SurrogateEscape => b.push(x),
                Errors::Replace => b.push(b'?'),
                Errors::Ignore => {}
                Errors::BackslashReplace => b.extend_from_slice(format!("\\u{cp:04x}").as_bytes()),
                Errors::XmlCharRefReplace => b.extend_from_slice(format!("&#{cp};").as_bytes()),
                Errors::SurrogatePass => b.extend_from_slice(&[
                    0xED,
                    0x80 | ((cp >> 6) & 0x3F) as u8,
                    0x80 | (cp & 0x3F) as u8,
                ]),
                Errors::Strict | Errors::Unknown(_) => unreachable!("returned above"),
            }
        }
        Ok(b)
    }

    /// `_textiowrapper_writeflush`: the pending bytes are dropped even when the write fails.
    fn flush_pending(&mut self) -> Result<(), SmError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let b = std::mem::take(&mut self.pending);
        self.buffer_write(&b)
    }

    /// `BufferedWriter.write`.
    fn buffer_write(&mut self, b: &[u8]) -> Result<(), SmError> {
        if b.len() <= self.buf_size - self.buf.len() && b.len() < self.buf_size {
            self.buf.extend_from_slice(b);
            return Ok(());
        }
        self.flush_buffer()?;
        let mut written = 0;
        while b.len() - written >= self.buf_size {
            written += self.raw_write(&b[written..])?;
        }
        self.buf.extend_from_slice(&b[written..]);
        Ok(())
    }

    /// `_bufferedwriter_flush_unlocked`: bytes not written stay in the buffer.
    fn flush_buffer(&mut self) -> Result<(), SmError> {
        while !self.buf.is_empty() {
            let buf = std::mem::take(&mut self.buf);
            match self.raw_write(&buf) {
                Ok(n) => self.buf = buf[n..].to_vec(),
                Err(e) => {
                    self.buf = buf;
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SmError> {
        self.flush_pending()?;
        self.flush_buffer()
    }

    /// `FileIO.write`: one `write(2)`, retried on EINTR.
    fn raw_write(&mut self, b: &[u8]) -> Result<usize, SmError> {
        match &mut self.sink {
            #[cfg(test)]
            Sink::Mem(v, sizes) => {
                v.extend_from_slice(b);
                sizes.push(b.len());
                Ok(b.len())
            }
            Sink::Fd(fd) => loop {
                // SAFETY: `b` is a live slice of `b.len()` bytes.
                let n = unsafe { libc::write(*fd, b.as_ptr().cast(), b.len()) };
                if n >= 0 {
                    return Ok(n as usize);
                }
                let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if errno != libc::EINTR {
                    return Err(os_error(errno));
                }
            },
        }
    }
}

fn is_tty(fd: i32) -> bool {
    // SAFETY: isatty only reads the fd number.
    unsafe { libc::isatty(fd) == 1 }
}

/// `max(min(st_blksize, 8 MiB), DEFAULT_BUFFER_SIZE)`, as `open(fd, buffering=-1)` picks it.
fn buffer_size(fd: i32) -> usize {
    // SAFETY: `st` is a zeroed stat that fstat fills.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let blksize = if unsafe { libc::fstat(fd, &mut st) } == 0 {
        st.st_blksize as usize
    } else {
        0
    };
    blksize.clamp(DEFAULT_BUFFER_SIZE, MAX_BUFFER_SIZE)
}

/// Python's `OSError` for an errno, with the subclass Python picks and `[Errno N] <strerror>`.
pub fn os_error(errno: i32) -> SmError {
    let class = match errno {
        libc::EPIPE | libc::ESHUTDOWN => "BrokenPipeError",
        libc::ECONNRESET => "ConnectionResetError",
        libc::EAGAIN | libc::EALREADY | libc::EINPROGRESS => "BlockingIOError",
        libc::EINTR => "InterruptedError",
        libc::ENOENT => "FileNotFoundError",
        libc::EISDIR => "IsADirectoryError",
        libc::EACCES | libc::EPERM => "PermissionError",
        _ => "OSError",
    };
    // SAFETY: strerror returns a NUL-terminated static string.
    let text = unsafe { std::ffi::CStr::from_ptr(libc::strerror(errno)) }
        .to_string_lossy()
        .into_owned();
    SmError {
        base: Base::Os,
        class: class.into(),
        msg: format!("[Errno {errno}] {text}"),
    }
}

/// An argv element decoded the way Python decodes it (UTF-8 with surrogateescape).
pub fn decode_argv(arg: &OsStr) -> String {
    crate::py::str::decode_surrogateescape(arg.as_bytes())
}

/// `os.fsencode(s)` under surrogateescape: stand-in chars go back to their raw bytes.
pub fn fs_path(s: &str) -> PathBuf {
    let mut bytes = Vec::with_capacity(s.len());
    for c in s.chars() {
        match surrogate_byte(c) {
            Some(b) => bytes.push(b),
            None => bytes.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
        }
    }
    PathBuf::from(OsString::from_vec(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pipe: (read end, write end).
    fn pipe() -> (i32, i32) {
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two fds pipe writes.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        (fds[0], fds[1])
    }

    fn close(fd: i32) {
        // SAFETY: closes an fd this test owns.
        unsafe { libc::close(fd) };
    }

    /// The bytes readable now on a read end, without blocking.
    fn drain(fd: i32) -> Vec<u8> {
        // SAFETY: fcntl on an fd this test owns.
        unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) };
        let mut got = Vec::new();
        let mut buf = [0u8; 65536];
        loop {
            // SAFETY: reads into a live buffer of its own length.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                return got;
            }
            got.extend_from_slice(&buf[..n as usize]);
        }
    }

    /// stdout on a pipe write end (block-buffered), stderr on another (line-buffered), set up
    /// by the same code `Out::new` runs on fd 1 and fd 2.
    fn out_on(stdout_fd: i32, stderr_fd: i32) -> Out {
        Out::on_fds(stdout_fd, stderr_fd, None)
    }

    /// What stdout writes for `text` under a `PYTHONIOENCODING` value, or the error's text.
    fn stdout_with(ioencoding: Option<&str>, text: &str) -> Result<Vec<u8>, String> {
        let mut out = Out {
            stdout: Stream::mem(false, stdout_errors(ioencoding)),
            stderr: Stream::mem(true, Errors::BackslashReplace),
        };
        out.write(text)
            .and_then(|()| out.flush())
            .map_err(|e| e.python_text())?;
        let Sink::Mem(bytes, _) = &out.stdout.sink else {
            unreachable!()
        };
        Ok(bytes.clone())
    }

    #[test]
    fn pythonioencoding_picks_stdouts_error_handler() {
        // Python 3.15.1, `env -i PYTHONIOENCODING=<v> python -c "import sys; print(sys.stdout.errors)"`.
        let cases: &[(Option<&str>, Errors)] = &[
            (None, Errors::SurrogateEscape),
            (Some(""), Errors::SurrogateEscape),
            (Some(":"), Errors::SurrogateEscape),
            (Some("utf-8"), Errors::Strict),
            (Some("UTF-8"), Errors::Strict),
            (Some("utf-8:"), Errors::Strict),
            (Some("utf-8:strict"), Errors::Strict),
            (Some("utf-8:surrogateescape"), Errors::SurrogateEscape),
            (Some(":surrogateescape"), Errors::SurrogateEscape),
            (Some(":replace"), Errors::Replace),
            (Some("UTF8:replace"), Errors::Replace),
            (Some(":ignore"), Errors::Ignore),
            (Some("utf-8:backslashreplace"), Errors::BackslashReplace),
            (Some("utf-8:namereplace"), Errors::BackslashReplace),
            (Some("utf-8:xmlcharrefreplace"), Errors::XmlCharRefReplace),
            (Some("utf-8:surrogatepass"), Errors::SurrogatePass),
            (Some("utf-8:Strict"), Errors::Unknown("Strict".into())),
            (Some("utf-8:strict:x"), Errors::Unknown("strict:x".into())),
        ];
        for (value, want) in cases {
            assert_eq!(&stdout_errors(*value), want, "{value:?}");
        }
    }

    #[test]
    fn stdout_writes_a_stand_in_as_its_handler_says() {
        // Python 3.15.1: sys.stdout.write("x\udcff\udcfey\udc80\n") under each PYTHONIOENCODING
        // (a stand-in char U+10FD00+b is the lone surrogate U+DC00+b of byte b).
        let text = "x\u{10fdff}\u{10fdfe}y\u{10fd80}\n";
        let ok = |v: Option<&str>| stdout_with(v, text);
        assert_eq!(ok(None), Ok(b"x\xff\xfey\x80\n".to_vec()));
        assert_eq!(ok(Some(":replace")), Ok(b"x??y?\n".to_vec()));
        assert_eq!(ok(Some(":ignore")), Ok(b"xy\n".to_vec()));
        assert_eq!(
            ok(Some("utf-8:backslashreplace")),
            Ok(b"x\\udcff\\udcfey\\udc80\n".to_vec())
        );
        assert_eq!(
            ok(Some("utf-8:namereplace")),
            Ok(b"x\\udcff\\udcfey\\udc80\n".to_vec())
        );
        assert_eq!(
            ok(Some("utf-8:xmlcharrefreplace")),
            Ok(b"x&#56575;&#56574;y&#56448;\n".to_vec())
        );
        assert_eq!(
            ok(Some("utf-8:surrogatepass")),
            Ok(b"x\xed\xb3\xbf\xed\xb3\xbey\xed\xb2\x80\n".to_vec())
        );
    }

    #[test]
    fn a_strict_stdout_raises_and_writes_nothing() {
        let text = "x\u{10fdff}\u{10fdfe}y\u{10fd80}\n";
        let strict = "UnicodeEncodeError: 'utf-8' codec can't encode characters in position 1-2: surrogates not allowed";
        assert_eq!(stdout_with(Some("utf-8"), text), Err(strict.to_string()));
        assert_eq!(
            stdout_with(Some("utf-8:strict"), text),
            Err(strict.to_string())
        );
        assert_eq!(
            stdout_with(Some("utf-8"), "ab\u{10fdc3}").unwrap_err(),
            "UnicodeEncodeError: 'utf-8' codec can't encode character '\\udcc3' in position 2: surrogates not allowed"
        );
        assert_eq!(
            stdout_with(Some("utf-8"), "plain \u{e9}\u{1f600}\n"),
            Ok("plain \u{e9}\u{1f600}\n".as_bytes().to_vec())
        );
    }

    #[test]
    fn a_strict_stdout_fd_gets_no_bytes_for_a_stand_in() {
        let (r, w) = pipe();
        let (er, ew) = pipe();
        let mut out = Out::on_fds(w, ew, Some("utf-8"));
        assert_eq!(
            out.print("a\u{10fdff}").unwrap_err().class,
            "UnicodeEncodeError"
        );
        out.print("b").unwrap();
        assert_eq!(out.finish(0), 0);
        assert_eq!(drain(r), b"b\n");
        for fd in [r, w, er, ew] {
            close(fd);
        }
    }

    #[test]
    fn an_unknown_handler_fails_only_when_a_stand_in_is_written() {
        assert_eq!(
            stdout_with(Some("utf-8:foo"), "a\u{10fdff}").unwrap_err(),
            "LookupError: unknown error handler name 'foo'"
        );
        assert_eq!(
            stdout_with(Some("utf-8:foo"), "ascii\n"),
            Ok(b"ascii\n".to_vec())
        );
    }

    #[test]
    fn stdout_on_a_pipe_holds_output_until_exit() {
        let (r, w) = pipe();
        let (er, ew) = pipe();
        let mut out = out_on(w, ew);
        assert_eq!(out.stdout.buf_size, DEFAULT_BUFFER_SIZE);
        out.print(&"x".repeat(19_999)).unwrap();
        assert_eq!(drain(r).len(), 0);
        assert_eq!(out.finish(0), 0);
        assert_eq!(drain(r).len(), 20_000);
        assert_eq!(drain(er).len(), 0);
        for fd in [r, w, er, ew] {
            close(fd);
        }
    }

    #[test]
    fn a_write_past_the_buffer_goes_straight_to_the_fd() {
        let (r, w) = pipe();
        let (er, ew) = pipe();
        let mut out = out_on(w, ew);
        let reader = std::thread::spawn(move || {
            let mut got = 0usize;
            let mut buf = [0u8; 65536];
            loop {
                // SAFETY: reads into a live buffer of its own length.
                let n = unsafe { libc::read(r, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    return got;
                }
                got += n as usize;
                if got == 200_000 {
                    return got;
                }
            }
        });
        out.write(&"y".repeat(200_000)).unwrap();
        assert_eq!(reader.join().unwrap(), 200_000);
        assert!(out.stdout.buf.is_empty() && out.stdout.pending.is_empty());
        for fd in [r, w, er, ew] {
            close(fd);
        }
    }

    /// The `raw.write` sizes CPython 3.15.1 makes for text writes of these sizes, then a flush:
    /// recorded with an `io.RawIOBase` that logs each call, under
    /// `TextIOWrapper(BufferedWriter(raw, 131072))`.
    #[test]
    fn write_sizes_match_cpython() {
        let cases: &[(&[usize], &[usize])] = &[
            (&[123072, 8000, 1000], &[123072, 9000]),
            (&[8000, 1000], &[9000]),
            (&[8000, 193], &[8193]),
            (&[130000, 1100], &[130000, 1100]),
            (&[131071, 1], &[131072]),
            (&[131072], &[131072]),
            (&[131073], &[131073]),
            (&[10000; 20], &[130000, 70000]),
            (&[5000, 5000, 5000], &[15000]),
        ];
        for (writes, expected) in cases {
            let mut out = Out::capture();
            for n in *writes {
                out.write(&"x".repeat(*n)).unwrap();
            }
            out.flush().unwrap();
            let Sink::Mem(_, sizes) = &out.stdout.sink else {
                unreachable!()
            };
            assert_eq!(sizes, expected, "writes {writes:?}");
        }
        let mut out = Out::capture();
        for _ in 0..3000 {
            out.write(&"y".repeat(99)).unwrap();
        }
        out.flush().unwrap();
        let Sink::Mem(_, sizes) = &out.stdout.sink else {
            unreachable!()
        };
        assert_eq!(sizes, &[123255, 123255, 50490]);
    }

    #[test]
    fn a_write_of_exactly_the_buffer_size_is_not_buffered() {
        let (r, w) = pipe();
        let (er, ew) = pipe();
        close(r);
        let mut out = out_on(w, ew);
        assert!(
            out.write_bytes(&vec![b'x'; DEFAULT_BUFFER_SIZE - 1])
                .is_ok()
        );
        assert!(out.write_bytes(b"y").is_ok());
        assert_eq!(out.write_bytes(b"z").unwrap_err().class, "BrokenPipeError");
        let mut out = out_on(w, ew);
        assert_eq!(
            out.write_bytes(&vec![b'x'; DEFAULT_BUFFER_SIZE])
                .unwrap_err()
                .class,
            "BrokenPipeError"
        );
        assert!(out.stdout.buf.is_empty());
        for fd in [w, er, ew] {
            close(fd);
        }
    }

    #[test]
    fn closed_pipe_at_exit_flush_gives_120_and_the_unraisable_text() {
        let (r, w) = pipe();
        let (er, ew) = pipe();
        close(r);
        let mut out = out_on(w, ew);
        out.print("small").unwrap();
        assert_eq!(out.finish(0), 120);
        assert_eq!(
            String::from_utf8(drain(er)).unwrap(),
            "Exception ignored while flushing sys.stdout:\nBrokenPipeError: [Errno 32] Broken pipe\n"
        );
        for fd in [w, er, ew] {
            close(fd);
        }
    }

    #[test]
    fn closed_pipe_on_a_direct_write_fails_in_the_run_and_leaves_nothing_buffered() {
        let (r, w) = pipe();
        let (er, ew) = pipe();
        close(r);
        let mut out = out_on(w, ew);
        let e = out.print(&"z".repeat(1_000_000)).unwrap_err();
        assert_eq!(
            (e.base, &*e.class, e.msg.as_str()),
            (Base::Os, "BrokenPipeError", "[Errno 32] Broken pipe")
        );
        assert_eq!(out.finish(1), 1);
        assert_eq!(drain(er).len(), 0);
        for fd in [w, er, ew] {
            close(fd);
        }
    }

    #[test]
    fn closed_pipe_on_a_buffer_flush_keeps_the_bytes_for_the_exit_flush() {
        let (r, w) = pipe();
        let (er, ew) = pipe();
        close(r);
        let mut out = out_on(w, ew);
        let line = "y".repeat(99);
        let e = (0..2000)
            .find_map(|_| out.print(&line).err())
            .expect("a flush fails");
        assert_eq!(e.class, "BrokenPipeError");
        assert_eq!(out.finish(1), 120);
        assert_eq!(
            String::from_utf8(drain(er)).unwrap().lines().last(),
            Some("BrokenPipeError: [Errno 32] Broken pipe")
        );
        for fd in [w, er, ew] {
            close(fd);
        }
    }

    #[test]
    fn stdout_on_a_tty_is_line_buffered_and_on_a_pipe_is_not() {
        let (mut master, mut slave) = (0, 0);
        // SAFETY: openpty fills the two fds; the name, termios and winsize pointers may be null.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0);
        let (r, w) = pipe();
        assert!(is_tty(slave));
        assert!(!is_tty(w));
        let mut out = Out::on_fds(slave, w, None);
        out.write("ab").unwrap();
        assert_eq!(drain(master), b"");
        out.print("c").unwrap();
        assert_eq!(drain(master), b"abc\r\n");
        for fd in [master, slave, r, w] {
            close(fd);
        }
    }

    #[test]
    fn stderr_is_line_buffered() {
        let (r, w) = pipe();
        let (er, ew) = pipe();
        let mut out = out_on(w, ew);
        out.ewrite("abc").unwrap();
        assert_eq!(drain(er), b"");
        out.ewrite("\n").unwrap();
        assert_eq!(drain(er), b"abc\n");
        for fd in [r, w, er, ew] {
            close(fd);
        }
    }

    /// Python 3.15.1 (`print('x\udcff'); print('y\udcfe', file=sys.stderr)` under the uv
    /// interpreter) writes `x 0xFF \n` to stdout and `y\udcfe\n` to stderr.
    #[test]
    fn stand_ins_are_raw_bytes_on_the_stdout_fd_and_backslash_escapes_on_the_stderr_fd() {
        let (r, w) = pipe();
        let (er, ew) = pipe();
        let mut out = out_on(w, ew);
        out.print(&decode_argv(OsStr::from_bytes(b"x\xff")))
            .unwrap();
        out.write(&decode_argv(OsStr::from_bytes(b"\xfe\xfd\xc3\xa9c")))
            .unwrap();
        out.eprint(&decode_argv(OsStr::from_bytes(b"y\xfe")))
            .unwrap();
        assert_eq!(out.finish(0), 0);
        assert_eq!(drain(r), b"x\xff\n\xfe\xfd\xc3\xa9c");
        assert_eq!(drain(er), b"y\\udcfe\n");
        for fd in [r, w, er, ew] {
            close(fd);
        }
    }

    #[test]
    fn captured_streams_encode_stand_ins_as_the_fds_do() {
        let mut out = Out::capture();
        out.print(&decode_argv(OsStr::from_bytes(b"ab\xff")))
            .unwrap();
        out.eprint(&decode_argv(OsStr::from_bytes(b"ab\xff")))
            .unwrap();
        out.finish(0);
        let Sink::Mem(bytes, _) = &out.stdout.sink else {
            unreachable!()
        };
        assert_eq!(bytes, b"ab\xff\n");
        assert_eq!(out.captured().1, "ab\\udcff\n");
    }

    #[test]
    fn argv_bytes_round_trip_through_stand_ins() {
        let raw: &[u8] = b"a\xc3\xa9\xff\xc3(\xed\xa0\x80";
        let s = decode_argv(OsStr::from_bytes(raw));
        assert_eq!(
            s.chars().filter(|&c| surrogate_byte(c).is_some()).count(),
            5
        );
        assert!(s.starts_with("a\u{e9}"));
        assert_eq!(fs_path(&s).into_os_string().into_vec(), raw);
        assert_eq!(decode_argv(OsStr::new("plain é")), "plain é");
        assert_eq!(repr(&s), "'aé\\udcff\\udcc3(\\udced\\udca0\\udc80'");
    }

    #[test]
    fn os_error_names_python_subclass_and_strerror() {
        let e = os_error(libc::ENOSPC);
        assert_eq!(
            (&*e.class, e.msg.as_str()),
            ("OSError", "[Errno 28] No space left on device")
        );
        assert_eq!(os_error(libc::EPIPE).class, "BrokenPipeError");
    }
}
