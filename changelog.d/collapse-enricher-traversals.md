- Indexing walks a function body fewer times. `MetricsEnricher` counted
  returns, gotos, string literals and throws in four separate bounded scans of
  the same body and now gathers all four in one; `EscapeEnricher` collected the
  locals, the array-typed locals and the static locals in three scans with the
  same node filter and now collects them in one. Measured on a 32,967-file C
  corpus against an untouched reference bucket, the metrics enricher costs 42%
  less and the escape enricher 21% less. Enrichment values are unchanged.
