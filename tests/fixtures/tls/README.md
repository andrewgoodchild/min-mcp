# TLS test fixtures

A throwaway PKI for the TLS and mutual-TLS tests, generated once with `openssl`
and committed so the suites need no toolchain, no network, and no generation
step.

| File | What it is |
|---|---|
| `ca.crt` / `ca.key` | Test CA. Signs both leaves; used as `client_ca_file` for mutual TLS |
| `server.crt` / `server.key` | Server leaf, `CN=localhost`, SAN `DNS:localhost,IP:127.0.0.1`, EKU `serverAuth` |
| `client.crt` / `client.key` | Client leaf, `CN=test-gateway`, EKU `clientAuth` |

**These keys are public.** They are in a public git history, they protect
nothing, and they must never be used outside these tests. All three expire in
2126, so the suites do not start failing on an expiry date.

Regenerate:

```sh
openssl req -x509 -newkey rsa:2048 -keyout ca.key -out ca.crt -days 36500 -nodes \
  -subj "/CN=min-mcp test CA" -addext "basicConstraints=critical,CA:TRUE"
openssl req -newkey rsa:2048 -keyout server.key -out server.csr -nodes -subj "/CN=localhost"
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -out server.crt -days 36500 \
  -extfile <(printf "subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\nbasicConstraints=CA:FALSE\n")
openssl req -newkey rsa:2048 -keyout client.key -out client.csr -nodes -subj "/CN=test-gateway"
openssl x509 -req -in client.csr -CA ca.crt -CAkey ca.key -CAcreateserial -out client.crt -days 36500 \
  -extfile <(printf "extendedKeyUsage=clientAuth\nbasicConstraints=CA:FALSE\n")
rm -f server.csr client.csr ca.srl
```
