# A Vitess node: MySQL 8.0 with Vitess's binaries beside it (vitess/lite no longer ships mysqld).
FROM vitess/lite:latest AS vt
FROM mysql:8.0
COPY --from=vt /vt/bin /vt/bin
COPY --from=vt /vt/config /vt/config
COPY durable.cnf /vt/config/mycnf/kilndb-durable.cnf
ENV PATH=/vt/bin:$PATH VTROOT=/vt VTDATAROOT=/vt/vtdataroot EXTRA_MY_CNF=/vt/config/mycnf/kilndb-durable.cnf
RUN mkdir -p /vt/vtdataroot && chown -R mysql:mysql /vt/vtdataroot
USER mysql
