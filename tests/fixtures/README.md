Test-only fixtures. `test_id_token_key.pem` is a throwaway RSA key generated
for these tests; it signs mock ID tokens that the tests verify against
`test_jwks.json`. It has no relation to OpenAI or any real account and must
never be used outside the test suite.
