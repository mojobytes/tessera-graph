# TesseraGraph Community

Base de datos de grafos embebible, con servidor Bolt.

Este repositorio aloja la edición **Community**: el motor de grafos, el
intérprete de consultas completo (lectura, escritura y transacciones),
autenticación local de usuario y contraseña, servidor Bolt con TLS, copia de
seguridad en frío y auditoría básica.

## Estado

Funcional. El servidor y la herramienta de administración se construyen y se
ejecutan; la suite de pruebas está en verde.

| Paquete | Qué es |
| --- | --- |
| `tessera-graph` | Motor de grafos: almacenamiento, índices, transacciones |
| `tessera-graph-cypher` | Intérprete del lenguaje de consulta |
| `tessera-graph-protocol` | Codificación del protocolo Bolt |
| `tessera-graph-server` | Servidor Bolt (binario `tessera-graph-server`) |
| `tessera-graph-cli` | Herramienta de administración (binario `tessera-graph-cli`) |
| `tessera-graph-config` | Lectura de configuración |
| `tessera-graph-python` | Enlace para Python |

Requisitos: Rust 1.88 o superior, edición 2024.

## Arranque rápido

El servidor **exige TLS**: sin certificado no arranca. Es deliberado — un
servidor de base de datos que acepta conexiones en claro por omisión es un
fallo de seguridad, no una comodidad.

Genera un certificado autofirmado para pruebas locales:

```bash
openssl req -x509 -newkey rsa:4096 -nodes -days 365 \
  -keyout server.key -out server.crt \
  -subj "/CN=localhost"
```

Arranca el servidor:

```bash
export TESSERA_TLS_CERT=$PWD/server.crt
export TESSERA_TLS_KEY=$PWD/server.key
export TESSERA_DATA_DIR=$PWD/data
export TESSERA_BIND=127.0.0.1:7687
export TESSERA_PASSWORD='una-contraseña-larga'

cargo run --release --bin tessera-graph-server
```

Conéctate con cualquier cliente que hable Bolt 4.4 — por ejemplo el
controlador oficial de Neo4j para Python:

```python
from neo4j import GraphDatabase

driver = GraphDatabase.driver(
    "bolt+ssc://127.0.0.1:7687",       # +ssc: acepta certificado autofirmado
    auth=("tessera", "una-contraseña-larga"),
)

with driver.session() as session:
    session.run("CREATE (:Persona {nombre: 'Ada'})")
    for record in session.run("MATCH (p:Persona) RETURN p.nombre AS nombre"):
        print(record["nombre"])
```

## Diferencias con Neo4j

El servidor habla Bolt 4.4 y acepta el controlador oficial de Neo4j sin
adaptaciones, pero el intérprete de consultas no es un clon: sigue el estándar
GQL más de cerca y exige explícito lo que Neo4j deduce. Lo que sigue está
comprobado contra un servidor en marcha, no derivado de la documentación.

### Hay que nombrar la base de datos al abrir la sesión

Neo4j usa una por omisión; aquí la primera consulta falla si no se indica. La
edición Community sirve una sola base, llamada `neo4j`.

```python
driver.session(database="neo4j")   # obligatorio
```

### Una cláusula de escritura por consulta

Encadenar varias en la misma sentencia da error de sintaxis. Se envían por
separado, o se crean las relaciones buscando antes los nodos.

```cypher
-- No admitido
CREATE (:Persona {nombre: 'Ada'}) CREATE (:Persona {nombre: 'Alan'})

-- Equivalente admitido: dos consultas, y las aristas con MATCH delante
CREATE (:Persona {nombre: 'Ada'})
CREATE (:Persona {nombre: 'Alan'})
MATCH (p:Persona), (c:Campo) CREATE (p)-[:TRABAJO_EN]->(c)
```

### Agrupar exige `GROUP BY` explícito

Al mezclar un valor por el que se agrupa con una función de agregación, Neo4j
infiere la agrupación; aquí hay que escribirla.

```cypher
-- No admitido
MATCH (p:Persona)-[:TRABAJO_EN]->(c:Campo) RETURN c.nombre, count(p)

-- Admitido
MATCH (p:Persona)-[:TRABAJO_EN]->(c:Campo)
RETURN c.nombre AS campo, count(p) AS cuantos GROUP BY c.nombre
```

Una agregación sola, sin valor que la acompañe, no necesita nada:
`MATCH (n) RETURN count(*)` funciona tal cual.

### Otras restricciones del intérprete

| Caso | Comportamiento |
| --- | --- |
| `CREATE (:A:B)` | Varias etiquetas por nodo no están admitidas; una por nodo |
| `CREATE (a)<-[:R]-(b)` | En `CREATE` solo se admiten aristas salientes (`-[:R]->`); se invierte el patrón |
| `MATCH … CREATE … RETURN` | No se admite: los nodos creados por esa vía no quedan disponibles para proyectar. `CREATE (n) RETURN n` a secas sí funciona |
| `UNWIND` sin ámbito previo | Necesita algo que lo enmarque delante: `UNWIND range(1,3) AS i CREATE (…)` funciona; `UNWIND [1,2,3] AS x RETURN x` suelto, no |
| `SKIP` | No reconocido; `ORDER BY` y `LIMIT` sí |
| `SHOW INDEXES`, `EXPLAIN` | No reconocidos por el intérprete de consultas |
| `CALL proc()` sin `YIELD` | La invocación de procedimientos exige `YIELD <columna>` |

Nombres de parámetro: evita los que coinciden con palabras reservadas. `$min`
falla al analizarse porque `min` es una función de agregación; `$umbral` con el
mismo valor funciona. Se admiten tanto por nombre (`$umbral`) como por posición
(`$1`, numerados desde uno), en igualdades y en desigualdades.

### Lo que sí se comporta como en Neo4j

Recorridos de longitud variable (`-[:R*1..2]->`), `OPTIONAL MATCH`, `WITH` como
tubería entre etapas, `MERGE`, `SET`, `DETACH DELETE`, `ORDER BY … LIMIT`,
`STARTS WITH`, los predicados sobre listas (`ALL`, `ANY`, `NONE`, `SINGLE`),
funciones como `toLower` y `coalesce`, la creación de índices y las
transacciones explícitas con confirmación y deshacer.

Dentro de una transacción explícita no se admiten sentencias de definición de
esquema, invocación de procedimientos ni administración: hay que confirmarla o
deshacerla primero.

## Configuración

Los ajustes se leen de `/etc/tessera/tessera.toml` (ruta por omisión) o de
variables de entorno, que tienen prioridad. Las más habituales:

| Variable | Para qué sirve |
| --- | --- |
| `TESSERA_BIND` | Dirección y puerto de escucha |
| `TESSERA_DATA_DIR` | Carpeta donde se guardan los datos |
| `TESSERA_TLS_CERT` / `TESSERA_TLS_KEY` | Certificado y clave (obligatorios) |
| `TESSERA_PASSWORD` | Contraseña del usuario administrador inicial |
| `TESSERA_METRICS_ADDR` | Dirección donde se publican las métricas |
| `TESSERA_QUERY_TIMEOUT_MS` | Tope de tiempo por consulta |
| `TESSERA_MAX_CONNECTIONS` | Tope de conexiones simultáneas |
| `TESSERA_AUDIT_ENABLED` | Activa el registro de auditoría |

Hay alrededor de cuarenta ajustes más para topes de recursos, limitación de
tráfico, rotación del registro de auditoría y mantenimiento en segundo plano.

## Desarrollo

```bash
cargo check --workspace --all-targets
cargo test --workspace --exclude tessera-graph-python --features plain-tcp
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

La opción `plain-tcp` habilita un canal sin cifrar **reservado a las pruebas
de integración**: sin ella, los binarios de prueba que necesitan una conexión
directa se compilan vacíos y la suite pasa sin haber ejercitado nada. El
binario que se publica nunca la activa.

Los bindings de Python se prueban sobre Python 3.9 y 3.12. Para construirlos y
ejecutar sus pruebas en un entorno virtual:

```bash
python3 -m venv .venv
.venv/bin/pip install 'maturin>=1.7,<2.0' pytest
.venv/bin/maturin develop --locked \
  --manifest-path crates/tessera-graph-python/Cargo.toml
.venv/bin/python -m pytest crates/tessera-graph-python/tests -q
```

La construcción reproducible y aislada está disponible mediante Docker:

```bash
docker build --target test \
  -f crates/tessera-graph-python/Dockerfile .
```

Antes de crear una versión se ejecuta `scripts/check-release.sh`. Una etiqueta
`vX.Y.Z` cuyo valor coincida con la versión del workspace activa la creación de
los binarios Community, los wheels de Python y el paquete del crate MIT. Los
artefactos se adjuntan automáticamente a una release de GitHub; la publicación
en crates.io o PyPI requiere después las credenciales y aprobación del titular.
Los cambios se documentan en [CHANGELOG.md](CHANGELOG.md).

## Licencias

El reparto de licencias entre componentes es parte del propio diseño:

- **Motor** (`tessera-graph`) y bindings Python: MIT — publicable como paquete
  independiente para grafos en memoria.
- **Servidor Community y componentes de red**: BSL 1.1 — admite uso productivo
  interno, pero no DBaaS, redistribución/OEM ni productos competidores sin un
  acuerdo comercial. Cada versión cambia a Apache-2.0 cuatro años después de
  hacerse pública.

Las funcionalidades de la edición Enterprise (autorización fina, multi-base y
multi-inquilino, auditoría de cumplimiento, proveedores de identidad
corporativos, copia de seguridad en caliente) viven en un repositorio
separado y no forman parte de este.

---

BelowZero Security OU · [tesseradb.io](https://tesseradb.io)
