# Checkout

Checkout commands use their own credentials and never fall back to your account. Provide a session token through `VOLTAGE_CHECKOUT_TOKEN` and a stream token through `VOLTAGE_STREAM_TOKEN`, or read either from a private file or stdin with `--token-file PATH` / `--token-file -`. Pass `--origin https://shop.example.com` when the session requires a browser origin.

```sh
voltage checkout sessions get SESSION_ID --token-file ./session.token
voltage checkout events watch --token-file - < ./stream.token
```

`checkout sessions get` retries while the session projection is being created, following the API's retry hints. `checkout events watch` streams server-sent events as `outcome: event` envelopes until the connection closes or `--timeout` passes; it does not reconnect. Ctrl-C exits cleanly.

Related: [output reference](../reference/output.md).
