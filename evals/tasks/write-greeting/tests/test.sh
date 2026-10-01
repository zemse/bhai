#!/usr/bin/env bash
# Runs in the workspace after the agent and writes 1 or 0 to $REWARD_FILE.
if [ "$(cat hello.txt 2>/dev/null)" = "Hello, world!" ]; then
  echo 1 >"$REWARD_FILE"
else
  echo 0 >"$REWARD_FILE"
fi
