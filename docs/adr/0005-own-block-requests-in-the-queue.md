# Own block requests in the queue

The async/batch block queue will own request objects, including VirtIO request headers, response status, completion state, token metadata, and resource guards, rather than relying on caller stack objects that stay alive until a `done` flag changes. This is a harder refactor, but it is required for real multi-request queue depth, safe cancellation boundaries, and later user direct I/O where pin guards must live until device completion.
