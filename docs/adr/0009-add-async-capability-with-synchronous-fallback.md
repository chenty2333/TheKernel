# Add async capability with synchronous fallback

The async/batch block queue will be exposed as an optional capability rather than replacing the existing synchronous `BlockDriverOps` contract for every driver at once. VirtIO should implement the new capability and eventually route its synchronous methods through the owned-request path, while devices without queue support keep the existing synchronous behavior.
