-- Test account rights for the disposable Oracle Free container (see docker-compose.test.yml).
ALTER SESSION SET CONTAINER = FREEPDB1;
GRANT DBA TO talus;
GRANT DEBUG CONNECT SESSION, DEBUG ANY PROCEDURE TO talus;
GRANT EXECUTE ON SYS.DBMS_DEBUG TO talus;
