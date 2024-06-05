# CW20-Reflection Spec: A CW20 reflection token implementation

CW20-Reflection is a specification for creating reflection tokens based on CosmWasm.
The name and design is based on the CW20 standard, with modifications made to allow the application of taxes, burns, and reflections.

The specification is split into multiple sections, a contract may only
implement some of this functionality, but must implement the base.

## Note

As much as possible, the original CW20 standard has been left untouched. Instead, additional function signatures were added to allow for a "reflection" behavior.

## Design

The standard has been built in a way to utilise 2 contracts:

- Reflection treasury: Any reflection and taxes are processed in the treasury contract. The CW20 Taxed Token is the owner of the treasury. Developers are able to retrieve the reflected amounts out of the treasury, and separately airdrop the amounts to their users.
- CW20 Taxed Token: This contract is a modified version of the CW20 to allow tax-on-transfer to happen. All `ExecuteMsg` and `QueryMsg` are preserved. Additional function signatures have been added to cater for the taxation logic.


## Rules of engagement

Before we begin, it is important to understand the rules of engagement of the CW20-Reflection standard, so developers can plan around this to create unique mechanics:

- Amounts are taxed upon transfers. This means any usage of `transfer`, `transfer_from`, `send`, `send_from` messages will incur a tax on recipient amounts.
- When using `send` or `send_from`, the DEDUCTED AMOUNT is relayed via the Cw20ReceiveMsg. This means developers need not account for the deducted amount manually via their contracts.
- Whitelisted EOAs are exempt from taxes
- Anti-whale mechanism has been added to prevent over-transferring of too huge of a supply. This prevents wild fluctuations resulting from over auto-liquidity mechanisms
- Standard was built against DojoSwap's DEX/AMM, customisations can be coded in to utilise other DEX-es as well

### Messages

`Transfer{recipient, amount}` - Moves `amount` CW20 tokens from the `info.sender` account to the `recipient` account. This is designed to send to an address controlled by a private key and does not trigger any actions on the recipient if it is a contract.

`Send{contract, amount, msg}` - Moves `amount` CW20 tokens from the `info.sender` account to the `contract` account. `contract` must be an address of a contract that implements the `Receiver` interface. The msg will be passed to the recipient contract, along with the amount.
