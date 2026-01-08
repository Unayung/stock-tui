# stock-tui

A terminal-based stock portfolio tracker with real-time price data from Yahoo Finance.

![Rust](https://img.shields.io/badge/rust-%23000000.svg?style=flat&logo=rust&logoColor=white)
![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)
![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS-blue)

## Features

- Real-time stock prices from Yahoo Finance
- 30-day price chart in detail view (press Enter)
- Multiple portfolio support with combined view
- Taiwan (.TW) and US stock markets
- USD/TWD exchange rate conversion
- Gain/loss tracking with cost basis
- Sortable columns (price, change %, quantity, gain)
- Add, edit, and delete stocks
- Privacy mode to hide position details
- Vim-style keyboard navigation

## Requirements

- Rust 1.70+ (for building from source)
- macOS or Linux
- Internet connection (for fetching stock prices)

## Installation

### From source

```bash
git clone https://github.com/unayung/stock-tui.git
cd stock-tui
cargo build --release
```

The binary will be at `target/release/stock-tui`.

### Using cargo

```bash
cargo install --path .
```

## Usage

```bash
stock-tui
```

### Demo Mode

Run with sample portfolio data (no configuration needed):

```bash
DEMO=true stock-tui
```

This loads `demo.conf` with sample TW and US stocks for testing.

### Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `0` | View all portfolios combined |
| `1-9` | Switch to portfolio |
| `Tab` | Switch between TW/US sections |
| `j/k` or `↑/↓` | Navigate rows |
| `h/l` or `←/→`| Switch portfolios |
| `Enter` | View stock detail with 30-day chart |
| `a` | Add stock |
| `e` | Edit selected stock |
| `d` | Delete selected stock |
| `n` | Create new portfolio |
| `r` | Refresh prices |
| `H` | Toggle hide positions (privacy mode) |
| `p` | Sort by price |
| `c` | Sort by change % |
| `y` | Sort by quantity |
| `g` | Sort by gain |
| `G` | Sort by gain % |
| `q` | Quit |

## Configuration

Portfolios are stored in `~/.config/stock-tui/portfolios/` as `.conf` files.

### Portfolio Format

```
# Stock Portfolio Configuration
# Format: SYMBOL|Display Name|Description|Quantity|Cost Basis

# Taiwan Stocks
2330.TW|TSMC|Taiwan Semiconductor|100|580.5

# US Stocks
AAPL|Apple|Apple Inc|50|175.25
NVDA|NVIDIA|NVIDIA Corporation|25|450.00
```

### Adding Taiwan Stocks

Taiwan stock codes are auto-detected. Enter `2330` and it will be converted to `2330.TW`.

## Data Source

Stock prices are fetched from Yahoo Finance API:
- Prices are loaded on startup and cached for 60 seconds
- Press `Enter` on a stock to view 30-day price chart (historical data cached for 6 hours)
- Press `r` to refresh all prices

## License

MIT License - see [LICENSE](LICENSE) for details.
