# Stablecoin receives

Stablecoin receives credit a fixed USD principal to a USD wallet without node infrastructure.
The organization must have stablecoin receives enabled by Voltage.
This preview follows backend PR #2374 at `c19a77852aca5bc292987e057b50f00baee40d47`.

## Create a quote, then receive

Find the wallet and its line of credit with `voltage wallets list` and `voltage wallets get WALLET_ID`.
Use a test-network wallet and a matching deployment.
Non-mainnet wallets pair with sepolia, hoodi, or devnet deployments; mainnet wallets pair with mainnet deployments.
`DEPLOYMENT` is `usdc:<network>:<contract>:<decimals>` or `usdt:<network>:<contract>:<decimals>`.
Supported token networks are `ethereum_mainnet`, `ethereum_sepolia`, `ethereum_hoodi`, `solana_mainnet`, and `solana_devnet`.
This network list mirrors backend `c19a7785`; new networks require a CLI update.
Ethereum contract text requires lowercase hex; mixed-case EIP-55 text is rejected by the backend.
The deployment contract and decimals must identify the actual token. A ticker alone is insufficient.

These commands create resources and require `--execute`:

```sh
voltage quotes create --credit-line CREDIT_LINE_ID --network mutinynet \
  --amount 20 --unit usd --to DEPLOYMENT --execute
voltage quotes get QUOTE_ID
voltage payments receive --wallet WALLET_ID --currency usd --kind stablecoin \
  --amount 20 --unit usd --quote QUOTE_ID --wait ready --qr --execute
```

Wait until the quote is visible before creating the receive. The CLI does not create quotes automatically.
The quote must be unexpired and match the exact USD principal and token deployment.
The receive derives its deployment from the quote; do not pass a separate deployment.
Amounts use USD or cents. Omit `description`, `expiration`, and `required_confirmations`; the backend rejects them.
The convenience interface rejects these flags before submission. Raw `--data` remains available and is validated by the backend.

## Read the payment

```sh
voltage payments get PAYMENT_ID --wait ready --copy
voltage payments get PAYMENT_ID --wait completed
voltage payments get PAYMENT_ID --json
```

`ready` requires `receiving` and a nonempty `data.address`. `completed` means the full quoted USD principal has been credited.
The address preserves case in clipboard and QR output. Completed payments return their result without QR or clipboard presentation.
Both QR layouts show the full token deployment (asset, network, contract, decimals) beside the address.
Missing or malformed deployment data suppresses QR and clipboard output until a valid deployment arrives.
Human token amounts retain that deployment text so networks and contracts remain distinguishable.
Stablecoin QR captions omit token amounts because cumulative settled units do not describe the remaining deposit.
Before the first deposit, `data.amount` is the target token amount. After deposits, it is cumulative settled token units.
Human output formats token base units using the deployment decimals, with the USD principal where the response exposes it.
Receipt history remains in the response, including both `Stablecoin` and `stablecoin` receipt tags.
Deposits after quote `expires_at`, and units beyond the target, remain in custody without credit.
A wait timeout does not mean settlement failed. Read the same payment ID to check its state.

A 403 `access_rights` response can indicate missing permission or organization allowlisting.
A 422 `invalid_data` response can indicate quote validation failure. Check the server's detail and quote before proceeding.
The CLI hints describe these checks; they never retry the mutation.

