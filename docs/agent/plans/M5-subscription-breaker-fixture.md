# Subscription breaker fixture flow

- Route a personal, loopback request through an explicitly remote test policy.
- Construct the runtime through the authorized fixed Claude sandbox profile.
- Use a repository executable linked into a private PATH; no vendor CLI, model,
  login state or network service is invoked.
- Three real fixture process failures return RELAY_CLI_FAILED and open the OAuth
  breaker. The fourth HTTP request must not launch another process.
- Check the runtime has no active request after each failure and shut it down.
- This is an HTTP-to-process wiring regression, not real vendor CLI identity,
  capability, startup or OS isolation validation. Owner decision #6 stays open.
