# Sandwich Maker ![license](https://img.shields.io/badge/License-MIT-green.svg?label=license)

A sandwich-making machine to perform Uniswap V2/V3 sandwich attacks written using Rust and Huff.

## Brief Explanation
Anytime that a transaction interacts with a Uniswap V2/V3 pool and its forks, there is some slippage introduced (routers, aggregators, other MEV bots). Bot will profit off this slippage by frontrunning the transaction pushing the price of an asset up to the slippage limit, and then immediately selling the asset through a backrun transaction.

**Bot Logic Breakdown** can be found under [bot/README.md](https://github.com/phureewat29/sandwich-maker/tree/master/bot)

**Contract Logic Breakdown** can be found under [contract/README.md](https://github.com/phureewat29/sandwich-maker/tree/master/contract)

## Features
- **Fully Generalized**: Sandwich any tx that introduces slippage.
- **V2 and V3 Logic**: Logic to handle Uniswap V2/V3 pools.
- **Multi-Meat**: Build and send multi-meat sandwiches.
- **Gas Optimized**: Contract written in Huff for gas optimizations.
- **Local Simulations**: Fast concurrent EVM simulations to find sandwich opportunities.
- **Salmonella Checks**: Detect if ERC20's transfer function uses any unusual opcodes that may produce different mainnet results.

> **Warning**
>
> **This software is highly experimental and should be used at your own risk.** Although tested, this bot is experimental software and is provided on an "as is" and "as available" basis under the MIT license. We cannot guarantee the stability or reliability of this codebase and are not responsible for any damage or loss caused by its use. We do not give out warranties.

## Acknowledgments
- [artemis](https://github.com/paradigmxyz/artemis)
- [foundry](https://github.com/foundry-rs/foundry)
- [huff-language](https://github.com/huff-language/huff-rs)
- [rusty-sando](https://github.com/mouseless-eth/rusty-sando)
