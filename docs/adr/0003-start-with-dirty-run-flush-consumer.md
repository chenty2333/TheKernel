# Start with dirty run flush as the first async consumer

The first async/batch block queue consumer will be page-cache dirty run flush, because kernel-owned dirty pages have simpler lifetimes than user direct I/O and still exercise real multi-request queue depth. The full plan will still include user direct I/O and the lwext4 read path as follow-on consumers of the same queue contract, rather than treating dirty flush as a special-purpose endpoint.
