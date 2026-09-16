# Attribution and scope

Chronicle is a product built around a layered history-management system. The
repository intentionally keeps the internal `hstry` crate names, configuration
directory, database path, adapter protocol, and data formats stable during the
public rename.

## Upstream engine

The history database, ingestion, search, export, and synchronization engine
started from [byteowlz/hstry](https://github.com/byteowlz/hstry). The upstream
MIT license and copyright notice are retained in [`LICENSE`](../LICENSE), and
substantial copies or distributions must continue to include them.

## Fork changes

The `andrew05060414/chronicle` fork contains changes made after forking,
including additional adapters, source discovery, remote hub/satellite
synchronization, resume/export integrations, backup and checkpoint workflows,
and platform-specific fixes. These changes remain part of the fork history and
are not presented as upstream work.

## Chronicle product structure and integration

Chronicle is the public product name and the user-designed structure around
the engine: the product boundary, documentation, supported workflows, and
integration choices that combine collection, search, citation-oriented
retrieval, synchronization, and backup. Chronicle does not claim that the
entire product originated upstream.

The public rename does not itself rename Rust crates, the `hstry` database,
configuration paths, adapter protocol, or existing data. Those compatibility
surfaces are deliberately preserved until a separately reviewed migration is
authorized.
