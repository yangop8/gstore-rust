# spec: SPARQL 1.1 Update §3.2.3 COPY: the service may fail when the input graph does
# not exist — this store does, unless SILENT (the existence rule it applies to
# CREATE, DROP and CLEAR too); the destination :g1 is not cleared
PREFIX : <http://example.org/>
COPY :none TO :g1
