# Xerxes

Xerxes is a high-performance filesystem navigation engine for macOS and Linux. It provides instant directory jumping using fuzzy matching and a frecency-based ranking algorithm.


## Installation

```bash
cargo install xerxes
```

## Setup

After installing, run the automatic setup command:

```bash
_xerxes setup
```

If your shell configuration files are in a non-standard location, you can specify the path manually:

```bash
_xerxes setup --path "~/.config/zsh/.zshrc"
```

This will detect your shell (zsh, bash, or fish) and add the necessary configuration to your shell profile. After running this, restart your terminal or source your config file.

## Usage

Use the `x` command followed by a query:

```bash
x backend       # Fuzzy search for a directory
x ..            # Go up one directory
x ~             # Go to home directory
x /             # Go to root
```

If multiple matches are found with similar scores, an interactive selector (FZF or custom TUI) will allow you to select the desired directory. Jumping to any directory (even one outside the index) makes it searchable from then on.

### Index Maintenance

New, renamed, and deleted directories are picked up live. To rebuild from scratch and drop stale or ignored entries:

```bash
x reindex   # full rescan; prunes vanished and ignored directories
x stats     # number of indexed directories
```

Hidden directories and common build/cache folders (`.git`, `node_modules`, `target`, `build`, `dist`, `venv`, `__pycache__`, `Library`, ...) are never indexed.

### Extra Roots

Only your home directory is indexed by default. To index other drives or mount points, list them in `XERXES_ROOTS` (`:`-separated), then restart the daemon and reindex:

```bash
export XERXES_ROOTS="/mnt/data:/media/usb"   # fish: set -Ux XERXES_ROOTS "/mnt/data:/media/usb"
pkill _xerxesd && _xerxes reindex
```

### Aliases

You can create permanent nicknames for directories to bypass fuzzy search entirely:

```bash
x alias . myproj                    # Alias current directory to "myproj"
x alias ~/work/very/deep/path boss  # Alias a deep path to "boss"
x alias --list                      # List all aliases
x alias --remove myproj             # Remove an alias
x alias --clear                     # Clear all aliases
x myproj                            # Jump instantly
```

### Smart Learning

Xerxes learns your preferences automatically. If you select a path from the interactive list for a specific query, Xerxes will remember that choice. The next time you use the same query, it will jump directly to that path without showing the list. Automatic picks are not remembered, so rankings keep adapting as your habits change.

To reset the learning memory:

```bash
x clear-memory
```
## Features

- **Performance**: Built with Rust using `redb` for storage and `nucleo-matcher` for SIMD-optimized fuzzy matching.
- **Frecency Ranking**: Combines visit frequency with a 7-day half-life decay so recently and frequently visited paths rank first.
- **Client-Daemon Architecture**: Uses a background daemon (`xerxesd`) over Unix domain sockets to eliminate cold-start latency.
- **Real-time Indexing**: Utilizes `notify` to track directory creation, renames, and deletions, batching bursts into single transactions.
- **Smart Jumping**: Directly supports special paths like `..`, `~`, `/`, and `-`.
- **Zero Configuration**: Automatically performs an initial reindex on first run.
- **Daemon-Backed**: Zero-latency search powered by a background indexer.
- **Dynamic Learning**: Remembers your selections and improves over time.
- **FZF Integration**: Uses FZF for selection if installed, with a robust TUI fallback.
- **TTY Safe**: Works perfectly inside shell subshells and aliases.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
