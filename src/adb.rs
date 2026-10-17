//! Modo USB: cada teléfono Android conectado por ADB recibe un
//! `adb reverse tcp:PORT tcp:PORT`, de modo que en el móvil
//! `http://localhost:PORT` llega a este servidor a través del cable.
//!
//! Las reglas de `adb reverse` se pierden cuando el teléfono se reconecta
//! (cambio de modo USB, reinicio del servidor adb…), así que en cada sondeo
//! se comprueba la lista real de reglas en lugar de recordar dispositivos.

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
    loop {
        for serial in list_devices().await {
            if has_reverse(&serial, &spec).await {
                continue;
            }
            let ok = adb(&["-s", &serial, "reverse", &spec, &spec]).await.is_some();
            if ok {
                tracing::info!("USB: {serial} conectado → abre la app o http://localhost:{port}");
                warned.remove(&serial);
            } else if warned.insert(serial.clone()) {
                tracing::warn!("USB: no se pudo configurar adb reverse en {serial} (¿autorizaste la depuración USB?)");
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
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
