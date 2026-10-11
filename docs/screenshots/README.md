# TUI screenshots

These PNGs capture the actual `yourai-tui` terminal output at 120 × 38 cells, replayed through a VT emulator and rendered with a monospace font. They are not UI mockups.

A local HTTP demo model drives a temporary project: the real tools create a task, read and edit `parser.py`, ask for shell approval, run two Python tests, and complete the task. No external model service or private session is used. `demo/code`, its variants, and the response text are demonstration fixtures.

- `conversation.png`: file diff, test output, Markdown reply, and composer; the task panel is collapsed.
- `approval.png`: approval required before running the test command.
- `models.png`: configured models, variants, and thinking effort.

To reproduce on macOS/Linux, from the repository root:

```sh
cargo build -p yourai-tui
python3 -m venv /tmp/yourai-screenshot-venv
/tmp/yourai-screenshot-venv/bin/pip install 'pyte==0.8.2' Pillow
/tmp/yourai-screenshot-venv/bin/python apps/yourai-tui/tests/capture_readme.py
```

The script chooses Menlo on macOS or DejaVu Sans Mono on Linux. Use `--font /path/to/monospace.ttf` for another font, or `--output /path/to/screenshots` to choose a destination. These Python packages are documentation tooling, not dependencies of the Rust application. Screenshot timing and temporary directory names may vary.
