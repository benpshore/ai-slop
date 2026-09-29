# Security Policy

Please report vulnerabilities privately via
**Security → Report a vulnerability** on this repository (GitHub private vulnerability reporting).
Do not open public issues for security problems.

## Model-provider threat model

Documents and prompts may contain confidential text, while provider keys grant
billable account access. The application therefore treats endpoint URLs, DNS,
redirects, response bodies, and logs as hostile boundaries. Its centralized
transport accepts the fixed HTTPS origins for OpenAI, Anthropic, and Gemini.
Custom HTTPS endpoints are structurally parsed (userinfo and fragments are
forbidden), resolved before use, and rejected if any answer is loopback,
private, link-local, multicast, or unspecified. Ollama may additionally use
plain HTTP only when every resolved address is IPv4 or IPv6 loopback; mixed or
empty DNS answers fail closed.

Credentials are bound to one provider and the validated origin. They cannot be
attached to a different provider or origin. Gemini authentication uses the
`x-goog-api-key` header rather than a URL parameter. Redirect following is
disabled, preventing authorization data from crossing origins. URLs, auth
headers, API-key parameters, signed URLs, document text, and provider bodies
must not be logged or copied into transport errors.

Requests have separate connection, per-request, and total deadlines. Request,
successful-response, and error-response bodies have explicit bounds; error
bodies are bounded before allocation and are never displayed.

## Local and cloud data flow

OpenAI, Anthropic, and Gemini are cloud policies: a request sends the selected
document context and question over TLS to that provider's fixed public origin.
Using one never sends another provider's credential. Ollama is the local policy:
plain HTTP is allowed only to a destination that resolves exclusively to the
local machine, so document data cannot silently travel to a LAN or public host.
An explicitly configured public Ollama-compatible HTTPS service is allowed only
after public-address validation; private-network HTTPS targets remain blocked.
