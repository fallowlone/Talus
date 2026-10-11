# PostgreSQL 17 with the pldebugger extension (PL/pgSQL debugger API) for integration tests.
FROM postgres:17
RUN apt-get update \
 && apt-get install -y --no-install-recommends postgresql-17-pldebugger \
 && apt-get clean
