#!/usr/bin/env bash
# Runs in the workspace after the agent and writes 1 or 0 to $REWARD_FILE.
if python3 -c '
from total import total
assert [total(n) for n in (0, 1, 2, 10, 100)] == [0, 1, 3, 55, 5050]
' 2>/dev/null; then
  echo 1 >"$REWARD_FILE"
else
  echo 0 >"$REWARD_FILE"
fi
