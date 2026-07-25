# TesseraGraph Community

Base de datos de grafos embebible, con servidor Bolt.

Este repositorio aloja la edición **Community**: el motor de grafos, el
intérprete de consultas completo (lectura, escritura y transacciones),
autenticación local de usuario y contraseña, servidor Bolt con TLS, copia de
seguridad en frío y auditoría básica.

## Estado

Repositorio recién inicializado. El código se incorpora por bloques desde la
línea de desarrollo principal; hasta que ese traslado termine, aquí no hay
todavía nada ejecutable.

## Licencias

El reparto de licencias entre componentes es parte del propio diseño:

- **Motor** (`tessera-graph`): Apache-2.0 — publicable como paquete
  independiente para grafos en memoria.
- **Servidor Community**: BSL 1.1 — código a la vista, no distribuible como
  servicio competidor.

Las funcionalidades de la edición Enterprise (autorización fina, multi-base y
multi-inquilino, auditoría de cumplimiento, proveedores de identidad
corporativos, copia de seguridad en caliente) viven en un repositorio
separado y no forman parte de este.

---

BelowZero Security OU · [tesseradb.io](https://tesseradb.io)
