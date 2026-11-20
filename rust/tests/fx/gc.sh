#!/bin/sh
# exits at once, leaves a grandchild that holds stdout for 8 s
( sleep 8 ) &
echo "Title here"
echo "- Done: x"
exit 0
