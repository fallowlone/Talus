#!/bin/sh
# Starts the disposable test databases and prints the env vars the integration tests read.
set -eu
cd "$(dirname "$0")/.."
# SQL Server only when its EULA was accepted explicitly (MSSQL_ACCEPT_EULA=Y); Oracle only on request
# (TALUS_ORACLE=1: a 2 GB container that needs a minute or two to start).
profiles=""
if [ "${MSSQL_ACCEPT_EULA:-}" = "Y" ]; then
  profiles="$profiles --profile mssql"
else
  echo "SQL Server skipped: set MSSQL_ACCEPT_EULA=Y to accept Microsoft's EULA and run it" >&2
fi
if [ "${TALUS_ORACLE:-}" = "1" ]; then
  profiles="$profiles --profile oracle"
else
  echo "Oracle skipped: set TALUS_ORACLE=1 to run Oracle Database Free" >&2
fi
# shellcheck disable=SC2086
docker compose -f docker-compose.test.yml $profiles up -d --wait >/dev/null
echo "export TALUS_TEST_PG=127.0.0.1:55432:talus:talus_test_pw:talus_test"
echo "export TALUS_TEST_MYSQL=127.0.0.1:53306:root:talus_test_pw:talus_test"
[ "${MSSQL_ACCEPT_EULA:-}" = "Y" ] && echo "export TALUS_TEST_MSSQL=127.0.0.1:51433:sa:Talus_Test_pw1:master"
[ "${TALUS_ORACLE:-}" = "1" ] && echo "export TALUS_TEST_ORACLE=127.0.0.1:51521:talus:talus_test_pw:FREEPDB1"
echo "export TALUS_TEST_REDIS=127.0.0.1:56379:default:redis_test_pw:0"
echo "export TALUS_TEST_MONGO=127.0.0.1:57017:root:mongo_test_pw:admin"
echo "export TALUS_TEST_SSH=127.0.0.1:52222:tunnel:tunnel_test_pw"
