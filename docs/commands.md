# API command registry

Generated from the checked-in specification and command registry. IDs are positional; enclosing scope uses flags.

| Command | Method | Path |
|---|---|---|
| `voltage assets list` | GET | `/organizations/{organization_id}/assets` |
| `voltage wallets list` | GET | `/organizations/{organization_id}/wallets` |
| `voltage wallets create` | POST | `/organizations/{organization_id}/wallets` |
| `voltage wallets get WALLET_ID` | GET | `/organizations/{organization_id}/wallets/{wallet_id}` |
| `voltage wallets delete WALLET_ID` | DELETE | `/organizations/{organization_id}/wallets/{wallet_id}` |
| `voltage wallets update WALLET_ID` | PATCH | `/organizations/{organization_id}/wallets/{wallet_id}` |
| `voltage wallets ledger list WALLET_ID` | GET | `/organizations/{organization_id}/wallets/{wallet_id}/ledger` |
| `voltage wallets policies get WALLET_ID` | GET | `/organizations/{organization_id}/wallets/{wallet_id}/policies` |
| `voltage wallets policies update WALLET_ID` | PATCH | `/organizations/{organization_id}/wallets/{wallet_id}/policies` |
| `voltage payments list` | GET | `/organizations/{organization_id}/environments/{environment_id}/payments` |
| `voltage payments create` | POST | `/organizations/{organization_id}/environments/{environment_id}/payments` |
| `voltage treasury movements create` | POST | `/organizations/{organization_id}/environments/{environment_id}/treasury_movements` |
| `voltage payments check` | POST | `/organizations/{organization_id}/environments/{environment_id}/payments/check` |
| `voltage payments summary` | GET | `/organizations/{organization_id}/environments/{environment_id}/payments/summary` |
| `voltage wallets payments summary WALLET_ID` | GET | `/organizations/{organization_id}/wallets/{wallet_id}/payments/summary` |
| `voltage payments get PAYMENT_ID` | GET | `/organizations/{organization_id}/environments/{environment_id}/payments/{payment_id}` |
| `voltage payments history PAYMENT_ID` | GET | `/organizations/{organization_id}/environments/{environment_id}/payments/{payment_id}/history` |
| `voltage credit-lines update-sandbox LINE_ID` | PATCH | `/organizations/{organization_id}/environments/{environment_id}/lines_of_credit/{line_id}/sandbox` |
| `voltage credit-lines get LINE_ID` | GET | `/organizations/{organization_id}/lines_of_credit/{line_id}/summary` |
| `voltage credit-lines list` | GET | `/organizations/{organization_id}/lines_of_credit/summaries` |
| `voltage bills get BILL_ID` | GET | `/organizations/{organization_id}/bills/{bill_id}` |
| `voltage bills list` | GET | `/organizations/{organization_id}/bills` |
| `voltage bills summary` | GET | `/organizations/{organization_id}/bills/summary` |
| `voltage webhooks list` | GET | `/organizations/{organization_id}/webhooks` |
| `voltage webhooks create` | POST | `/organizations/{organization_id}/environments/{environment_id}/webhooks` |
| `voltage webhooks get WEBHOOK_ID` | GET | `/organizations/{organization_id}/environments/{environment_id}/webhooks/{webhook_id}` |
| `voltage webhooks delete WEBHOOK_ID` | DELETE | `/organizations/{organization_id}/environments/{environment_id}/webhooks/{webhook_id}` |
| `voltage webhooks update WEBHOOK_ID` | PATCH | `/organizations/{organization_id}/environments/{environment_id}/webhooks/{webhook_id}` |
| `voltage webhooks keys rotate WEBHOOK_ID` | POST | `/organizations/{organization_id}/environments/{environment_id}/webhooks/{webhook_id}/keys` |
| `voltage webhooks test WEBHOOK_ID` | POST | `/organizations/{organization_id}/environments/{environment_id}/webhooks/{webhook_id}/test` |
| `voltage webhooks start WEBHOOK_ID` | POST | `/organizations/{organization_id}/environments/{environment_id}/webhooks/{webhook_id}/start` |
| `voltage webhooks stop WEBHOOK_ID` | POST | `/organizations/{organization_id}/environments/{environment_id}/webhooks/{webhook_id}/stop` |
| `voltage webhooks deliveries list` | GET | `/organizations/{organization_id}/environments/{environment_id}/webhooks/{webhook_id}/deliveries` |
| `voltage webhooks deliveries summary` | GET | `/organizations/{organization_id}/environments/{environment_id}/webhooks/{webhook_id}/deliveries/summary` |
| `voltage webhooks deliveries get DELIVERY_ID` | GET | `/organizations/{organization_id}/environments/{environment_id}/webhooks/{webhook_id}/deliveries/{delivery_id}` |
| `voltage webhooks deliveries abandon DELIVERY_ID` | POST | `/organizations/{organization_id}/environments/{environment_id}/webhooks/{webhook_id}/deliveries/{delivery_id}/abandon` |
| `voltage webhooks deliveries retry DELIVERY_ID` | POST | `/organizations/{organization_id}/environments/{environment_id}/webhooks/{webhook_id}/deliveries/{delivery_id}/retry` |
| `voltage quotes list` | GET | `/organizations/{organization_id}/environments/{environment_id}/quotes` |
| `voltage quotes create` | POST | `/organizations/{organization_id}/environments/{environment_id}/quotes` |
| `voltage quotes get QUOTE_ID` | GET | `/organizations/{organization_id}/environments/{environment_id}/quotes/{quote_id}` |
| `voltage checkout events watch` | GET | `/checkout/events` |
| `voltage checkout streams create` | POST | `/checkout/streams` |
| `voltage checkout settings get` | GET | `/organizations/{organization_id}/environments/{environment_id}/checkout/settings` |
| `voltage checkout settings update` | PUT | `/organizations/{organization_id}/environments/{environment_id}/checkout/settings` |
| `voltage checkout sessions create` | POST | `/organizations/{organization_id}/environments/{environment_id}/checkout/sessions` |
| `voltage checkout sessions get SESSION_ID` | GET | `/checkout/sessions/{session_id}` |
| `voltage checkout sessions allowed-origins SESSION_ID` | GET | `/checkout/sessions/{session_id}/allowed-origins` |
