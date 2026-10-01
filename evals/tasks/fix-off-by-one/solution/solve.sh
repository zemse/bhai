#!/usr/bin/env bash
python3 - <<'EOF'
from pathlib import Path
p = Path("total.py")
p.write_text(p.read_text().replace("range(1, n)", "range(1, n + 1)"))
EOF
