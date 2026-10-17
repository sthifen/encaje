//! Modo USB: cada teléfono Android conectado por ADB recibe un
//! `adb reverse tcp:PORT tcp:PORT`, de modo que en el móvil
//! `http://localhost:PORT` llega a este servidor a través del cable.
//!
//! Las reglas de `adb reverse` se pierden cuando el teléfono se reconecta
//! (cambio de modo USB, reinicio del servidor adb…), así que en cada sondeo
//! se comprueba la lista real de reglas en lugar de recordar dispositivos.
//!
//! La app del visor se abre sola en cada dispositivo al conectarlo.

use std::collections::HashSet;
use std::time::Duration;

use tokio::process::Command;

pub async fn watch(port: u16) {
    if Command::new("adb").arg("version").output().await.is_err() {
        tracing::warn!("adb no encontrado: el modo USB está desactivado (instala android-tools)");
        return;
    }
    let spec = format!("tcp:{port}");
    let mut warned: HashSet<String> = HashSet::new();
    // Dispositivos en los que ya se abrió la app (la regla puede venir de
    // una ejecución anterior y entonces no se crea aquí).
    let mut opened: HashSet<String> = HashSet::new();
    loop {
        for serial in list_devices().await {
            let fresh = !has_reverse(&serial, &spec).await;
            if fresh {
                if adb(&["-s", &serial, "reverse", &spec, &spec]).await.is_none() {
                    if warned.insert(serial.clone()) {
                        tracing::warn!("USB: no se pudo configurar adb reverse en {serial} (¿autorizaste la depuración USB?)");
                    }
                    continue;
                }
                tracing::info!("USB: {serial} conectado (http://localhost:{port})");
                warned.remove(&serial);
            }
            if fresh || opened.insert(serial.clone()) {
                open_app(&serial).await;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Paquete y actividad de la app del visor.
const APP: &str = "dev.spacedesk.linux";
const ACTIVITY: &str = "dev.spacedesk.linux/.MainActivity";

/// Abre la app del visor (si ya está abierta, la trae al frente).
async fn open_app(serial: &str) {
    match adb(&["-s", serial, "shell", "am", "start", "-n", ACTIVITY]).await {
        Some(out) if !out.contains("Error") => tracing::info!("USB: app del visor abierta en {serial}"),
        _ => tracing::warn!("USB: no se pudo abrir la app en {serial} (¿está instalada?); ábrela o ve a http://localhost"),
    }
}

/// Cierra la app del visor: en el modo sólo entrada sobra.
pub async fn close_app(serial: &str) {
    let _ = adb(&["-s", serial, "shell", "am", "force-stop", APP]).await;
}

pub async fn adb(args: &[&str]) -> Option<String> {
    let out = Command::new("adb").args(args).output().await.ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn has_reverse(serial: &str, spec: &str) -> bool {
    adb(&["-s", serial, "reverse", "--list"])
        .await
        .is_some_and(|list| list.lines().any(|l| l.split_whitespace().nth(1) == Some(spec)))
}

pub async fn list_devices() -> Vec<String> {
    let Some(out) = adb(&["devices"]).await else { return Vec::new() };
    out.lines()
        .skip(1)
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            match (parts.next(), parts.next()) {
                (Some(serial), Some("device")) => Some(serial.to_owned()),
                _ => None,
            }
        })
        .collect()
}
