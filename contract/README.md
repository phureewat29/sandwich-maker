# Sandwich Maker/Contract ![license](https://img.shields.io/badge/License-MIT-green.svg?label=license)

Gas-optimized sandwich contract written in Huff to make use of unconventional gas optimizations.

> Why not Yul? Yul does not give access to the stack or jump instructions.

## Gas Optimizations

### JUMPDEST Function Sig
Instead of reserving 4 bytes for a function selector, store a JUMPDEST in the first byte of calldata and jump to it at the beginning of execution. Doing so allows us to jump to the code range 0x00-0xFF, so we fill that range with place holder JUMPDEST that point to the location of the associated function body.

Example:
```as
#define macro MAIN() = takes (0) returns (0) {
    // extract function selector (JUMPDEST encoding)
    push0                                       // [0x00]
    calldataload                                // [calldata]
    push0                                       // [0x00, calldata]
    byte                                        // [jumplabel]
    jump                                        // []
```

> **Note**
> JUMPDEST 0xfa is reserved to handle [UniswapV3 callback](https://docs.uniswap.org/contracts/v3/reference/core/interfaces/callback/IUniswapV3SwapCallback).

### Encoding WETH Value Using tx.value
When dealing with WETH amounts, the amount is encoded by first dividing the value by 100000, and setting the divided value as `tx.value` when calling the contract. The contract then multiplies `tx.value` by 100000 to get the original amount.

> The last 5 digits of the original value are lost after encoding, however, it is a small amount of wei and can be ignored.

Example:
```as
    // setup calldata for swap(wethOut, 0, address(this), "")
    [V2_Swap_Sig] 0x00 mstore
    0x0186a0 callvalue mul 0x04 mstore          // original weth value is decoded here by doing `100000 * callvalue`
    0x00 0x24 mstore
    address 0x44 mstore
    0x80 0x64 mstore
```

### Encoding Other Token Value Using 5 Bytes Of Calldata
When dealing with the other token amount, the values can range significantly depending on the token decimal and total supply. To account for the full range, we encode by fitting the value into 4 bytes of calldata plus a byte shift. To decode, we byteshift the 4bytes to the left.

We use byte shifts instead of bitshifts because we perform a byteshift by storing the 4bytes in memory N bytes to the left of its memory slot.

To optimize further, instead of encoding the byteshift into our calldata, we encode the offset in memory such that when the 4bytes are stored, it will be N bytes from the left of its storage slot. [more details](https://github.com/phureewat29/sandwich-maker/blob/3b17b30340f6ef3558be5e505e55a1eb2fe8ca36/contract/test/misc/SandwichCommon.sol#L11).

### Hardcoded values
Weth address is hardcoded into the contract and there are individual methods to handle when Weth is token0 or token1.

### Encode Packed
All calldata is encoded by packing the values together.

> **Note**
> Free alfa: Might be able to optimize contract by eliminating unnecessary [memory expansions](https://www.evm.codes/about#memoryexpansion) by changing order that params are stored in memory. I did not account for this when writing the contract.

## Interface

| JUMPDEST | Function Name                          |
| :------: | :------------------------------------- |
|   0x05   | V2 Backrun, Weth is Token0 and Output  |
|   0x0A   | V2 Frontrun, Weth is Token0 and Input  |
|   0x0F   | V2 Backrun, Weth is Token1 and Output  |
|   0x14   | V2 Frontrun, Weth is Token1 and Input  |
|   0x19   | V3 Backrun, Weth is Token0 and Output  |
|   0x1E   | V3 Frontrun, Weth is Token0 and Input  |
|   0x23   | V3 Backrun (Weth is Token1 and Output) |
|   0x28   | V3 Frontrun, Weth is Token1 and Input  |
|   0x2D   | Seppuku (self-destruct)                |
|   0x32   | Recover Eth                            |
|   0x37   | Recover Weth                           |
|   ...    | ...                                    |
|   0xFA   | UniswapV3 Callback                     |


## Calldata Encoding (Interface)
### Uniswap V2 Calldata Encoding Format

#### Frontrun (weth is input)
| Byte Length | Variable                 |
| :---------: | :----------------------- |
|      1      | JUMPDEST                 |
|     20      | PairAddress              |
|      1      | Where to store AmountOut |
|      4      | EncodedAmountOut         |

#### Backrun(weth is output)
| Byte Length | Variable                |
| :---------: | :---------------------- |
|      1      | JUMPDEST                |
|     20      | PairAddress             |
|     20      | TokenInAddress          |
|      1      | Where to store AmountIn |
|      4      | EncodedAmountIn         |

### Uniswap V3 Calldata Encoding Format

#### Frontrun (weth is input)
| Byte Length | Variable    |
| :---------: | :---------- |
|      1      | JUMPDEST    |
|     20      | PairAddress |
|     32      | PoolKeyHash |
> PoolKeyHash used to verify that msg.sender is a uniswawp v3 pool in callback (protection)

#### Backrun (weth is output)
| Byte Length | Variable                |
| :---------: | :---------------------- |
|      1      | JUMPDEST                |
|     20      | PairAddress             |
|     20      | TokenInAddress          |
|     32      | PoolKeyHash             |
|      1      | Where to store AmountIn |
|      4      | EncodedAmountIn         |

> **Note**
> PairAddress can be omitted from calldata because it can be derived from PoolKeyHash

## Running Tests
```console
forge install
forge test
```
