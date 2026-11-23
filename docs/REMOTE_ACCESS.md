# Remote access

Keep the supervisor API on loopback. Use a separately administered trusted transport for remote access, with API authentication still required. Conductor does not establish a VPN, TLS tunnel or network access policy. Never expose its unauthenticated local transport directly to the Internet.

By default, browser origins are limited to local development/desktop origins, and API Hosts to loopback at the selected port. Operator-owned `<state>/http-access.json` can add exact deployment origins:

```json
{
  "https_origins": ["https://conductor.example.invalid"],
  "trusted_http_hosts": []
}
```

An HTTPS origin adds that exact Host and browser origin while retaining Secure session cookies. No wildcard, path, credential, query or fragment is allowed. Unknown fields and invalid/duplicate entries fail startup before listener/token changes. Restart the listener after a configuration change.

`trusted_http_hosts` is an explicit assertion that an external HTTP host is protected by a separately trusted transport. An entry such as `conductor.example.invalid:8788` permits only that exact Host/origin and relaxes Secure cookies only there. HTTP itself provides no encryption. Prefer HTTPS; no external hosts or cookie relaxation are enabled by default. Do not configure the same Host under both policies.

The edge gateway forwards only its allowed API/tool routes. Bearer authentication, principal/grant scope and paired-session CSRF checks remain separate from CORS and Host filtering. Keep tokens, pairing material, local host policy and transport configuration in private runtime state. Backend restart rotates its local token; browser sessions must reconnect.
