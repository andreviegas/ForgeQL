/**
 * motor_control.h — Subsistema de control de motores y sensores
 *
 * Desafíos para herramientas basadas en texto (grep / sed / regex):
 *
 *  1. "encender" es prefijo de encenderMotor Y encenderSistema.
 *     grep -r encender  →  falsos positivos inevitables.
 *
 *  2. encenderMotor aparece en comentarios y en literales de cadena.
 *     Una herramienta de texto lo renombraría; ForgeQL no.
 *
 *  3. La macro ARRANCAR() expande a encenderMotor(VELOCIDAD_MAX).
 *     grep busca llamadas directas; la macro queda sin renombrar.
 *
 *  4. Dos enums distintos tienen el miembro OK.
 *     s/OK/Success/g rompe ambos; ForgeQL renombra por alcance.
 *
 *  5. velocidad existe como campo de struct Y como variable local.
 *     Regex no distingue; el AST sí.
 *
 *  6. El símbolo está declarado aquí Y definido en motor_control.cpp.
 *     grep necesita dos pasadas; ForgeQL trabaja el workspace completo.
 */

#pragma once
#include <cstdint>

/* ------------------------------------------------------------------ */
/* Configuración / Configuration                                        */
/* ------------------------------------------------------------------ */

/* Trap 7: VELOCIDAD_MAX aparece en #define, en #ifdef, y como          *
 * argumento — sed lo cambia en los tres; constexpr solo en uno.        */
#define VELOCIDAD_MAX       255u
#define VELOCIDAD_MIN       0u
#define UMBRAL_TEMP_CRITICA 85u
#define LOG_SUBSISTEMA      "motor_ctrl"

/* Macro tipo función — expande a una llamada directa a encenderMotor.  *
 * Trap 3: grep 'encenderMotor' no encuentra este sitio de uso.         */
#define ARRANCAR()          encenderMotor(VELOCIDAD_MAX)
#define PARAR()             apagarMotor()

/* Macro de utilidad pura (sin side-effects).                           */
#define LIMITAR(v, lo, hi)  ((v) < (lo) ? (lo) : ((v) > (hi) ? (hi) : (v)))

/* ------------------------------------------------------------------ */
/* Tipos / Types                                                        */
/* ------------------------------------------------------------------ */

/**
 * Trap 4a: ErrorMotor tiene miembro OK.
 * Migrar a enum class requiere calificar TODOS los sitios de uso:
 *   OK  →  ErrorMotor::OK
 */
enum ErrorMotor {
    OK      = 0,   /**< Operación exitosa / Operation successful         */
    TIMEOUT = 1,   /**< Sin respuesta / No response                      */
    FALLO   = 2,   /**< Error de hardware / Hardware fault               */
};

/**
 * Trap 4b: ErrorSensor también tiene miembro OK — colisión de nombre.
 * sed 's/\bOK\b/Success/g' cambia ambos enums, rompiendo el código.
 */
enum ErrorSensor {
    OK        = 0, /**< Lectura correcta / Reading OK                    */
    SIN_DATOS = 3, /**< Sensor no disponible / Sensor unavailable        */
    SATURADO  = 4, /**< Fuera de rango / Out of range                    */
};

/**
 * Trap 5 + Trap 8: typedef struct estilo C antiguo.
 * El campo 'velocidad' comparte nombre con variables locales.
 * La migración a 'struct EstadoMotor' requiere actualizar todos los
 * usos de EstadoMotor_s en declaraciones, cast y sizeof.
 */
typedef struct {
    uint8_t  velocidad;    /**< Velocidad PWM 0–255 / PWM speed 0–255    */
    uint8_t  temperatura;  /**< Temperatura en °C / Temperature in °C    */
    uint8_t  estado;       /**< 0 = apagado, 1 = encendido               */
    char     etiqueta[16]; /**< Nombre del motor / Motor label            */
} EstadoMotor_s;

/** Tipo puntero a callback — usado para asignación directa de símbolo.  *
 * Trap 5b: 'FnCallback' aparece en typedef Y en declaración de var.    */
typedef void (*FnCallback)(void);

/* ------------------------------------------------------------------ */
/* Declaraciones de función / Function declarations                     */
/* ------------------------------------------------------------------ */

/**
 * encenderMotor — Activa el motor a la velocidad indicada.
 * Trap 1 + 2: el nombre aparece en este comentario Javadoc,
 *             en literales de cadena más abajo, y como substring
 *             de encenderSistema.
 */
void encenderMotor(uint8_t velocidad);

/** apagarMotor — Detiene el motor reduciendo velocidad a cero.         */
void apagarMotor(void);

/**
 * encenderSistema — Inicialización completa del subsistema.
 * Trap 1: grep 'encender' afecta esta línea aunque no sea encenderMotor.
 */
void encenderSistema(void);

/** ajustarVelocidad — Cambia la velocidad sin apagar el motor.         */
void ajustarVelocidad(uint8_t nueva);

/** leerTemperatura — Lee la temperatura del motor principal.           */
ErrorMotor  leerTemperatura(uint8_t *out);

/** leerSensor — Lee un sensor por índice; usa ErrorSensor.             */
ErrorSensor leerSensor(uint8_t id, uint8_t *out);

/** registrarCallback — Registra una función a llamar al encender.      */
void registrarCallback(FnCallback fn);

/** reiniciarSistema — Apaga y vuelve a encender vía macro ARRANCAR().  */
void reiniciarSistema(void);
