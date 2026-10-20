# Webhooks

```sh
voltage webhooks create --profile prod --name orders \
  --url https://example.com/voltage --event receive.completed \
  --output-file webhook.json --execute
```

A webhook sends events from one environment to your URL. Creating one returns its shared secret once, so the CLI refuses to run without `--output-file PATH` or `--show-secrets`. `--output-file` saves the complete response, including the secret, to a new owner-only file; `--show-secrets` prints it, so avoid it in shared terminals and logs. See [secrets and redaction](../concepts/safety-model.md#secrets-and-redaction).

To see the environment's webhooks and one webhook's settings:

```sh
voltage webhooks list --profile prod
voltage webhooks get WEBHOOK_ID --profile prod
```

## Choose events

`--event` is repeatable and names an event as `KIND.TYPE`:

| Kind | Types |
|---|---|
| `receive` | `generated`, `detected`, `refreshed`, `expired`, `succeeded`, `completed`, `failed` |
| `send` | `succeeded`, `failed` |
| `test` | `created` |

`webhooks update` replaces the whole selection, so pass every event you still want:

```sh
voltage webhooks update WEBHOOK_ID --profile prod \
  --event receive.completed --event send.failed --execute
```

## Test, pause, and resume

A test delivery takes a body with a new `delivery_id` and a sample event `payload`. The smallest is a `test.created` event, for example in `test-delivery.json`:

```json
{
  "delivery_id": "1f0b5c2e-3a4d-4e8f-9b6a-7c2d1e0f4a5b",
  "payload": {
    "type": "test",
    "detail": { "event": "created", "data": "Checking the endpoint" }
  }
}
```

A `receive` or `send` payload carries a whole payment instead; the API reference has an example of each. Stopping pauses deliveries until you start the webhook again:

```sh
voltage webhooks test WEBHOOK_ID --profile prod --data @test-delivery.json --execute
voltage webhooks stop WEBHOOK_ID --profile prod --execute
voltage webhooks start WEBHOOK_ID --profile prod --execute
```

## Rotate the secret

Rotation also returns a new secret once, so it needs `--output-file` too:

```sh
voltage webhooks keys rotate WEBHOOK_ID --profile prod \
  --output-file webhook-key.json --execute
```

## Deliveries

Deliveries belong to a webhook, selected with `--webhook`:

```sh
voltage webhooks deliveries summary --profile prod --webhook WEBHOOK_ID
voltage webhooks deliveries list --profile prod --webhook WEBHOOK_ID --all
voltage webhooks deliveries retry DELIVERY_ID --profile prod --webhook WEBHOOK_ID --execute
voltage webhooks deliveries abandon DELIVERY_ID --profile prod --webhook WEBHOOK_ID --execute
```

To verify a delivery's signature in your receiver, see the [Voltage API webhook docs](https://voltageapi.com/v1/docs).

Related: [requests](requests.md), [scripting](scripting.md), [safety model](../concepts/safety-model.md).
