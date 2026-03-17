/**
 * motor_control.cpp — Implementación del subsistema de motores y sensores.
 *
 * Este archivo es el "campo de pruebas" de ForgeQL.
 * Cada sección está etiquetada con la fase de test que la ejercita:
 *
 *   Fase 1  — RENAME symbol (simple, multi-archivo, función como valor)
 *   Fase 2a — MIGRATE define → constexpr
 *   Fase 2b — MIGRATE enum  → enum class  (con colisión de miembros)
 *   Fase 2c — MIGRATE typedef struct → struct
 *   Fase 3  — TRANSLATE comments FROM 'es' TO 'en'
 *   Fase 4  — ADD attribute [[nodiscard]], REMOVE dead code
 */

#include "motor_control.h"
#include <cstring>
#include <cstdio>

/* ------------------------------------------------------------------ */
/* Estado global / Global state                                         */
/* ------------------------------------------------------------------ */

static EstadoMotor_s motorPrincipal;    /* Fase 2c: EstadoMotor_s → EstadoMotor  */
static EstadoMotor_s motorSecundario;
/* static const — internal linkage; must NOT appear in FIND globals     */
static const char* kMotorLabel = "motor_principal";

/**
 * Trap 5b: puntero a función cuyo valor inicial ES el símbolo
 * encenderMotor — al renombrar la función, esta línea también debe
 * cambiar, pero grep 'encenderMotor(\s*(' no coincide aquí porque
 * no hay paréntesis de llamada.
 */
static FnCallback gCallbackEncendido = encenderMotor;

/* ------------------------------------------------------------------ */
/* Fase 1 — RENAME encenderMotor → startMotor                          */
/*                                                                      */
/* Apariciones de 'encenderMotor' en este archivo:                      */
/*   1. forward decl  (arriba, en el .h)                                */
/*   2. definición    (función real, más abajo)                         */
/*   3. asignación a puntero  (gCallbackEncendido línea de arriba)      */
/*   4. dentro de la macro ARRANCAR() en el .h                          */
/*   5. literal de cadena     ← NO debe renombrarse                     */
/*   6. comentario en código  ← NO debe renombrarse                     */
/* ------------------------------------------------------------------ */

void encenderMotor(uint8_t velocidad)
{
    /* Trap 5: 'velocidad' aquí es el parámetro; abajo será campo.      *
     * sed 's/velocidad/speed/g' renombraría campo y variable local.    */
    uint8_t vel = LIMITAR(velocidad, VELOCIDAD_MIN, VELOCIDAD_MAX);

    motorPrincipal.velocidad = vel;   /* campo, no la variable local    */
    motorPrincipal.estado    = 1u;

    /* Trap 2: literal de cadena — ForgeQL NO debe renombrar esto.      */
    (void)printf("[%s] encenderMotor: velocidad=%u\n", LOG_SUBSISTEMA, vel);

    if (gCallbackEncendido != nullptr) {
        gCallbackEncendido();
    }
}

void apagarMotor(void)
{
    /* Comentario: llamar ajustarVelocidad antes de cortar energía.     */
    ajustarVelocidad(0u);

    motorPrincipal.velocidad = VELOCIDAD_MIN;
    motorPrincipal.estado    = 0u;

    /* Trap 2: mismo nombre en comentario — NO es una llamada real.     *
     * Una búsqueda de texto encontraría "encenderMotor" aquí:          *
     * "Para volver a activar, usar encenderMotor(VELOCIDAD_MIN)."      */
}

/**
 * Trap 1 + Trap 3: encenderSistema llama a encenderMotor directamente
 * Y mediante la macro ARRANCAR() que también expande a encenderMotor.
 * grep -n encenderMotor no ve la llamada vía macro.
 */
void encenderSistema(void)
{
    encenderMotor(VELOCIDAD_MIN);   /* llamada directa — visible a grep  */

    motorPrincipal.estado    = 1u;
    motorSecundario.estado   = 1u;

    std::memset(motorPrincipal.etiqueta,   0, sizeof(motorPrincipal.etiqueta));
    std::memset(motorSecundario.etiqueta,  0, sizeof(motorSecundario.etiqueta));
    std::strncpy(motorPrincipal.etiqueta,  "principal",  15);
    std::strncpy(motorSecundario.etiqueta, "secundario", 15);

    if (gCallbackEncendido != nullptr) {
        gCallbackEncendido();
    }
}

/* ------------------------------------------------------------------ */
/* Fase 2a — MIGRATE define VELOCIDAD_MAX → constexpr                  */
/* ------------------------------------------------------------------ */

void ajustarVelocidad(uint8_t nueva)
{
    /* Trap 5: variable local 'velocidad' y campo struct 'velocidad'     *
     * tienen exactamente el mismo nombre. Renombrar el campo requiere   *
     * conocer el tipo — regex no puede distinguir.                      */
    uint8_t velocidad = LIMITAR(nueva, VELOCIDAD_MIN, VELOCIDAD_MAX);

    motorPrincipal.velocidad  = velocidad;   /* campo  */
    motorSecundario.velocidad = velocidad;   /* campo  */

    /* Trap 2a: VELOCIDAD_MAX en un literal de cadena de depuración.    */
    (void)printf("[%s] velocidad ajustada a %u (max=%u)\n",
                 LOG_SUBSISTEMA, velocidad, VELOCIDAD_MAX);

#ifdef VELOCIDAD_MAX
    /* Trap 7: VELOCIDAD_MAX en una directiva #ifdef.                   *
     * migrate define → constexpr debe actualizar la #ifdef también,    *
     * o el bloque quedará siempre activo / inactivo.                   */
    (void)printf("[%s] límite de velocidad activo\n", LOG_SUBSISTEMA);
#endif
}

/* ------------------------------------------------------------------ */
/* Fase 2b — MIGRATE enum ErrorMotor → enum class; ErrorSensor ídem   */
/*                                                                      */
/* Trap 4: ambos enums tienen miembro OK.                               */
/* Después de la migración:                                             */
/*   OK        →  ErrorMotor::OK  (dentro de leerTemperatura)          */
/*   OK        →  ErrorSensor::OK (dentro de leerSensor)               */
/*   FALLO     →  ErrorMotor::FALLO                                     */
/*   TIMEOUT   →  ErrorMotor::TIMEOUT                                   */
/*   SIN_DATOS →  ErrorSensor::SIN_DATOS                               */
/* grep 's/\bOK\b/Success/g' no puede saber cuál es cuál.              */
/* ------------------------------------------------------------------ */

ErrorMotor leerTemperatura(uint8_t *out)
{
    if (out == nullptr) {
        return FALLO;     /* ErrorMotor::FALLO después de la migración  */
    }

    *out = motorPrincipal.temperatura;

    if (motorPrincipal.temperatura > UMBRAL_TEMP_CRITICA) {
        apagarMotor();
        return TIMEOUT;   /* ErrorMotor::TIMEOUT después               */
    }

    return OK;            /* ErrorMotor::OK — misma palabra que abajo  */
}

ErrorSensor leerSensor(uint8_t id, uint8_t *out)
{
    if (out == nullptr) {
        /* Trap 4c: cast cruzado entre enums — ForgeQL necesita         *
         * análisis de tipos para dejar el cast correcto.               */
        return static_cast<ErrorSensor>(FALLO);
    }

    switch (id) {
        case 0u:
            *out = motorPrincipal.temperatura;
            return OK;        /* ErrorSensor::OK — mismo token que arriba  */

        case 1u:
            *out = motorSecundario.temperatura;
            return OK;        /* ErrorSensor::OK                           */

        default:
            return SIN_DATOS; /* ErrorSensor::SIN_DATOS                   */
    }
}

/* ------------------------------------------------------------------ */
/* Fase 2c — MIGRATE typedef struct EstadoMotor_s → struct EstadoMotor */
/*                                                                      */
/* Trap 8: sizeof(EstadoMotor_s) y cast (EstadoMotor_s *) también       */
/* deben actualizarse. grep 'EstadoMotor_s' los encuentra, pero no     */
/* distingue contexto de tipos vs usos de variable.                    */
/* ------------------------------------------------------------------ */

static void imprimirEstado(const EstadoMotor_s *m)
{
    if (m == nullptr) return;

    (void)printf("[%s] estado: vel=%u temp=%u on=%u label=%s\n",
                 LOG_SUBSISTEMA,
                 m->velocidad,
                 m->temperatura,
                 m->estado,
                 m->etiqueta);

    /* Trap: sizeof con el nombre del typedef.                          */
    (void)printf("[%s] sizeof(EstadoMotor_s)=%zu\n",
                 LOG_SUBSISTEMA, sizeof(EstadoMotor_s));
}

/* ------------------------------------------------------------------ */
/* Fase 1 (continuación) — función de registro y reinicio              */
/* ------------------------------------------------------------------ */

void registrarCallback(FnCallback fn)
{
    gCallbackEncendido = fn;
}

/**
 * reiniciarSistema — Apaga el motor y vuelve a encenderlo.
 *
 * Trap 3: ARRANCAR() expande a encenderMotor(VELOCIDAD_MAX) en tiempo
 * de preprocesador. Un rename basado en texto que no entienda macros
 * dejará encenderMotor sin renombrar dentro de la macro.
 */
void reiniciarSistema(void)
{
    apagarMotor();
    ARRANCAR();    /* grep no ve 'encenderMotor' aquí — macro opaca     */
}

/* ------------------------------------------------------------------ */
/* Fase 4 — candidatos para ADD attribute y REMOVE dead code           */
/* ------------------------------------------------------------------ */

/**
 * calcularPotencia — Resultado ignorado a menudo en el código cliente.
 * Candidato para [[nodiscard]].
 * Fase 4: ADD attribute [[nodiscard]] TO function 'calcularPotencia'
 */
uint32_t calcularPotencia(uint8_t velocidad, uint8_t carga)
{
    return static_cast<uint32_t>(velocidad) * static_cast<uint32_t>(carga);
}

/* Código muerto / Dead code — candidato para REMOVE.                  *
 * Fase 4: REMOVE lines MATCHING 'DEBUG_LEGACY%' FROM 'motor_control.cpp' */
#define DEBUG_LEGACY_DUMP_MOTOR()   imprimirEstado(&motorPrincipal)
#define DEBUG_LEGACY_DUMP_SENSOR()  (void)printf("sensor legacy\n")
#define DEBUG_LEGACY_VERSION        "0.1-alpha"
