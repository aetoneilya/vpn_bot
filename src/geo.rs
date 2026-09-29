//! Offline IP geolocation (country, region, city, network operator) from DB-IP Lite
//! databases in MaxMind format. Used to attach "where and on which operator" to complaints.

use std::net::IpAddr;
use std::path::Path;

use anyhow::{Context, Result};
use maxminddb::{Mmap, Reader, geoip2};
use serde::Serialize;

use crate::config::GeoConfig;

pub struct GeoDb {
    city: Reader<Mmap>,
    asn: Reader<Mmap>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct GeoInfo {
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub asn: Option<u32>,
    /// Network operator as registered for the ASN, e.g. "PJSC MegaFon".
    pub operator: Option<String>,
}

impl GeoInfo {
    /// Human-readable one-liner, e.g. "PJSC MegaFon (AS31133) · Москва, Россия".
    pub fn summary(&self) -> String {
        let network = match (&self.operator, self.asn) {
            (Some(op), Some(asn)) => format!("{op} (AS{asn})"),
            (Some(op), None) => op.clone(),
            (None, Some(asn)) => format!("AS{asn}"),
            (None, None) => "оператор неизвестен".into(),
        };
        let place: Vec<&str> = [&self.city, &self.region, &self.country]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        if place.is_empty() {
            network
        } else {
            format!("{network} · {}", place.join(", "))
        }
    }
}

impl GeoDb {
    pub fn open(config: &GeoConfig) -> Result<Self> {
        Ok(Self {
            city: open(&config.city_db)?,
            asn: open(&config.asn_db)?,
        })
    }

    pub fn lookup(&self, ip: IpAddr) -> GeoInfo {
        let mut info = GeoInfo::default();

        if let Ok(Some(city)) = self
            .city
            .lookup(ip)
            .and_then(|r| r.decode::<geoip2::City>())
        {
            info.country = localized(&city.country.names);
            info.region = city.subdivisions.first().and_then(|s| localized(&s.names));
            info.city = localized(&city.city.names);
        }
        if let Ok(Some(asn)) = self.asn.lookup(ip).and_then(|r| r.decode::<geoip2::Asn>()) {
            info.asn = asn.autonomous_system_number;
            info.operator = asn.autonomous_system_organization.map(ToString::to_string);
        }
        info
    }
}

fn open(path: &Path) -> Result<Reader<Mmap>> {
    // SAFETY: the databases are only ever replaced by an atomic rename, so the mapped
    // file is never modified in place while the reader holds it.
    unsafe { Reader::open_mmap(path) }
        .with_context(|| format!("failed to open GeoIP database {}", path.display()))
}

fn localized(names: &geoip2::Names<'_>) -> Option<String> {
    names.russian.or(names.english).map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_formats_known_parts() {
        let info = GeoInfo {
            country: Some("Россия".into()),
            region: None,
            city: Some("Москва".into()),
            asn: Some(31133),
            operator: Some("PJSC MegaFon".into()),
        };
        assert_eq!(info.summary(), "PJSC MegaFon (AS31133) · Москва, Россия");
        assert_eq!(GeoInfo::default().summary(), "оператор неизвестен");
    }
}
