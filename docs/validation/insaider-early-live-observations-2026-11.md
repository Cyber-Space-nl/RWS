# Early live-use observations: insaider.nl on RWS

A point-in-time account based on snapshots captured on 16 and 17 November 2026. These are historical observations, not a statement about the current deployment.

## Deployment and observation window

At the time, insaider.nl served a PHP application through RWS and PHP 8.5-FPM, with native TLS, automatic
HTTP-to-HTTPS redirects and a protected phpMyAdmin application under `/database/`.

The observed binary was a source build at
[`d86e197`](https://github.com/Cyber-Space-nl/RWS/commit/d86e19752e3c74a3c720a21eb1205106c2e8edf1),
including the split-Cookie FastCGI fix. Its version string is beta.7; the original beta.7
download predates that fix. Results apply to this deployed build and its configuration.

The complete log snapshot covers 13:41–17:45 CET and includes 593 requests across deployment
changes and checks. For the observed process, the observation window starts at 15:16:29 CET,
with 304 logged requests. Traffic includes administrative use, page assets, polling, test
requests and automated traffic. Request totals do not establish a visitor count.

## Observed operation

| Measurement | Observation |
|---|---|
| Observed process window | Approximately 2 hours 29 minutes |
| Automatic service restarts | 0 |
| RWS resident memory at capture | 14.6 MiB RSS; 12.1 MiB PSS |
| Cumulative RWS CPU time since start | 1.73 seconds |
| Observed process site requests | 304 |
| HTTP 200 responses in that window | 276 |
| Median logged duration for HTTP 200 | 23.584 ms |
| p95 logged duration for HTTP 200 | 34.583 ms |
| Maximum logged duration for HTTP 200 | 45.803 ms |
| Logged HTTP 5xx responses | 0 in the complete 593-request snapshot |
| RWS journal warnings/errors since process start | 0 |

The duration is the server's recorded request-dispatch time until response construction.
It includes PHP-FPM work for PHP requests and excludes connection setup and full response
transfer. These observations describe low traffic on a shared VPS; they do not establish
throughput limits, capacity under load or long-term memory stability.

## What the error log contains

Both access and error files were active. The error snapshot has 72 lines, all HTTP 4xx
responses: 49 HTTP 404, 15 HTTP 403 and 8 HTTP 401. There were no additional explicit
FastCGI/proxy error messages in that file.

After the observed process started, the site log contains:

| Status | Count | Context |
|---|---:|---|
| 200 | 276 | Successful responses |
| 303 | 6 | Application redirects after POST requests |
| 304 | 14 | Cache validation |
| 308 | 2 | Permanent redirects |
| 401 | 1 | Protected `/database/` without credentials |
| 403 | 2 | Rejected admin POST requests |
| 404 | 3 | Requests for `/robots.txt` |

Earlier scan traffic requested paths such as `.env`, AWS configuration and PHP unit-test
endpoints; the observed responses were 404. The global error counter also includes rejected
unknown hosts, which do not appear in the per-site access log. HTTP error counters therefore
represent more than application/server failures.

## Session-related observation

The split-Cookie problem had been reproduced and fixed separately: HTTP/2 Cookie fields were
being overwritten during FastCGI conversion. Combining them with `; ` before conversion
preserved the PHP session in an isolated real HTTP/2/PHP-FPM test. The source suite passed
77 tests for that build.

Two admin POST requests still returned 403 at 16:53:12 and 16:53:26 CET. A POST at 16:53:34
returned 303, followed by further successful GETs and POST redirects. There were no later
403s by the 17:45 capture. The access log does not expose the response body, CSRF decision or
authentication outcome, so the exact cause of those two rejections remains undetermined.

## Later observation: a separate process and build

A second snapshot was captured on 17 November at 11:03 CET, after a planned deployment and
restart at 18:14:53 CET on 16 November. It concerns source build
[`df5d729`](https://github.com/Cyber-Space-nl/RWS/commit/df5d729), which added Referer logging,
not the earlier `d86e197` process. Counts in the two windows must not be combined as one run.

An analysis through 11:01 CET counted 3,103 site requests since that process started, including
100 HTTP 503 responses. All 100 occurred within 13 ms at 02:06:27 during a scan from one client;
PHP-FPM also reported reaching its `max_children=4` limit at that time. The precise mechanism
behind the 503 responses was not established. No further 503 responses appeared in the examined
window. There were no automatic service restarts or RWS journal warnings since this process
started. This event is not a controlled capacity test.

## Interpretation and publication scope

The first observation showed no logged 5xx responses or automatic restarts during its light
workload. The evidence supports an initial operational account for one deployment. Further
observation is needed for broader performance or reliability claims, and the two admin 403s
should remain disclosed rather than classified as a proven recurrence or dismissal of the
Cookie issue.

This report contains aggregated results. Private source logs, client IPs, query strings and
service metadata are retained separately and are not intended as public report attachments.
