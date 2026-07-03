# Drain completions opportunistically first

The first async block queue implementation will not introduce a permanent completion worker. Completion draining will happen from submit, wait, queue-full, and interrupt paths first, because a default polling worker could hide busy-wait CPU cost in another task and complicate scheduling before the core queue contract is proven.
