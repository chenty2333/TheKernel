# Use conservative barriers for first async writes

The first async write consumer will batch ordinary dirty data writes, but metadata, truncate, fsync, sync, close, and flush boundaries will fence earlier writes before continuing. This deliberately avoids building a request reordering scheduler before the queue contract is proven, and prevents page-cache dirty state from being cleared before the corresponding block request completes successfully.
