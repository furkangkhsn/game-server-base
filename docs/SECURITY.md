# gsb: Güvenlik Turu Tasarımı — TLS, Rate-Limit, Pre-Auth Sınırlar

> Durum: TASARIM (iki uygulama turu bu dokümanı sözleşme alır: A=TLS,
> B=sınırlar). Dış inceleme ailesinin kalan teknik maddeleri burada
> kapanır: şifreleme, auth rate-limit, pre-auth tahsis sınırı.

## 1. Kapsam ve statü

| Madde | Tur | Durum |
|---|---|---|
| TCP üstüne TLS (rustls) | Tur A | ✅ Uygulandı |
| Auth rate-limit + pre-auth amplifikasyon sınırı | Tur B | ✅ Uygulandı |
| Pre-auth oturum tahsis sınırı | Tur B | ✅ Uygulandı |
| rUDP cookie rotasyonu (yakalanan proof'un son kullanma tarihi) | rUDP doğruluk turu | ✅ Uygulandı (DESIGN §5, "Cookie rotasyonu"; slot = 10 sn, pencere 10-20 sn) |
| rUDP şifreleme/congestion | Kapsam DIŞI — rUDP deneysel statüde; kanıtlanmış taşıma ya da ayrı tur |
| Admin HTTP auth | OPS.md NOT-DONE (localhost sözleşmesi) |

## 2. TLS (Tur A)

### Kararlar

| # | Karar | Gerekçe |
|---|---|---|
| 1 | **rustls**, native-tls değil | Saf Rust, ring/aws-lc arka planı; platform OpenSSL bağımlılığı yok — projenin bağımlılık disiplinine uyar |
| 2 | Yeni transport: `TlsTransport` (`gsb-net`) | `Transport`/`Listener`/`Endpoint` soyutlamasının ilk gerçek karşılığı: accept sonrası TLS handshake, ardından MEVCUT uzunluk-prefix framing değişmeden akar. Aktör katmanına sıfır dokunuş |
| 3 | Config: `tls_cert`, `tls_key` (PEM yolları). İkisi boş = plaintext (varsayılan, davranış değişmez); tekisi dolu = başlatma HATASI | Sessiz zayıf geri düşüş yok — rUDP cookie-key kararının aynı ilkesi |
| 4 | İstemci tarafı: loadgen/example'a `--tls-ca <PEM>` bayrağı | Self-signed senaryo dahil uçtan uca doğrulanabilsin |
| 5 | Test sertifikaları: `rcgen` **yalnız dev-dependency** ile koşu-anında üretilir | Repoya sertifika kommitlemek yerine her test kendi mini-PKI'sını kurar; runtime bağımlılığı sıfır kalır |
| 6 | ALPN/ad doğrulama: istemci server adını config'den alır (`--tls-server-name`, varsayılan "localhost") | Test sertifikaları localhost SAN'lı üretilir |
| 7 | rUDP TLS almaz | Deneysel statü (udp.rs beyanıyla tutarlı); `transport = "udp"` + tls_cert birlikte istenirse başlatma hatası |

### Uçlar

- Handshake yavaş/düşmanca istemci: TLS accept timeout (config'siz sabit,
  örn. 10 sn) aşımında bağlantı kapatılır — reader-pump idle idiom'u ile
  aynı aile
- `TlsTransport` plaintext `tcp` ile aynı e2e süitinden geçer
  (parametreli: her guardrail testi iki transportta da koşar — rUDP'de
  yapılanın TLS karşılığı)
- Performans: loopback ölçümü yük turu geleneğine göre ROADMAP'e yazılır
  (beklenti: el sıkışma maliyeti bağlantı başina bir kez; tick yolu
  değişmez)

## 3. Rate-limit + amplifikasyon sınırı (Tur B)

### Kararlar

| # | Karar | Gerekçe |
|---|---|---|
| 1 | **AUTH deneme limiti:** bağlantı başına 10 sn pencerede 3 deneme; aşımı HARD-violation (bütçe puanı) | Mevcut ihlal-bütçesi makinesinin yeniden kullanımı — yeni mekanizma yok; meşru istemci 3 denemede asla aşmaz |
| 2 | **Pre-auth HEARTBEAT yanıtı:** auth öncesi en fazla 1/sn cevap; fazlası sessizce sayılır (RACE-violation değil, sayaç) | HEARTBEAT 1:1 cevap amplifikasyonunun kapatılması; auth sonrası heartbeat dokunulmaz (liveness sinyali) |
| 3 | **Pre-auth toplam frame bütçesi:** auth başarısına kadar toplam N=64 frame; aşımında bağlantı kapanır (ERROR 9) | Auth etmeden sonsuz kontrol-frame üretebilmenin kapatılması; N meşru el sıkışmayı (AUTH+JOIN+heartbeat'ler) fazlasıyla karşılar |
| 4 | Hepsi bağlantı actor'ünün yerel durumunda — kilit/kanal eklenmez | Mevcut violation-budget ile aynı desen |

## 4. Pre-auth tahsis sınırı (Tur B)

| # | Karar | Gerekçe |
|---|---|---|
| 1 | Registry, bağlantı tablosunda **auth durumu** tutar (`ConnOpened` unauthed açar; AUTH başarısı bildirilir) | Cap'in görüleceği tek yer registry'nin tablosudur (bağlantı sayısı orada yaşar — mevcut ilke) |
| 2 | **Unauthed cap:** `max_connections * %25` (min 64); aşan yeni bağlantı ERROR 9 ile nazikçe reddedilir | Script'li handshake fırtınasının (çok IP'li) bellek büyütmesini sınırlar; meşru yavaş-auth akışı için bol pay |
| 3 | Sayımlar aktör-local sayaçlarla; detach/resume bu sınıfa girmez (resume ticket'lıdır, authed sayılır) | RECONNECT semantiği korunur |

## 5. Test planı

Tur A: TLS ile tüm guardrail e2e'leri (plaintext parametresiyle parametrik);
self-signed handshake; yanlış CA reddi; tek-taraf-config başlatma hatası;
handshake-timeout kapanması.
Tur B: auth flood → bütçe tükenimi + kapanma; pre-auth heartbeat
fırtınası → sessiz sayaç; 64-frame aşımı → ERROR 9; unauthed cap
dolunca yeni bağlantının reddi + authed olanın etkilenmemesi.

## 6. NOT-DONE

- mTLS (istemci sertifikası) — ticket-auth yeterli v1'de
- TLS 0-RTT/session resumption ayarları — varsayılanlar
- Admin HTTP auth/TLS — OPS.md NOT-DONE devam
- rUDP crypto — deneysel statü
