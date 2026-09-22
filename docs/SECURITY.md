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
| Post-auth HEARTBEAT_ACK kısması (§3.2'nin ikinci yarısı) | Bağlantı sınırları turu | ✅ Uygulandı |
| Tıkanmış yazmaya süre sınırı (`write_stall_secs`) | Bağlantı sınırları turu | ✅ Uygulandı (§3.5); **bayt-granüler** (stall gözlemlenebilirliği turu) |
| Sunucu-başlatımlı kapanış sayaçları, sebep bazında | Stall gözlemlenebilirliği turu | ✅ Uygulandı (§3.6) |
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
| 2 | **HEARTBEAT yanıtı (her iki faz):** en fazla 1/sn cevap; fazlası sessizce sayılır (RACE-violation değil, sayaç) | HEARTBEAT 1:1 cevap amplifikasyonunun kapatılması. **GÜNCELLENDİ** (bağlantı sınırları turu): kural artık auth sonrasını da kapsıyor — aşağıya bakınız |
| 3 | **Pre-auth toplam frame bütçesi:** auth başarısına kadar toplam N=64 frame; aşımında bağlantı kapanır (ERROR 9) | Auth etmeden sonsuz kontrol-frame üretebilmenin kapatılması; N meşru el sıkışmayı (AUTH+JOIN+heartbeat'ler) fazlasıyla karşılar |
| 4 | Hepsi bağlantı actor'ünün yerel durumunda — kilit/kanal eklenmez | Mevcut violation-budget ile aynı desen |

### 3.2'nin ikinci yarısı — post-auth kısma (bağlantı sınırları turu)

Tur B'nin kararı yalnız `WaitingAuth` fazına bağlıydı; auth başarısından
sonra her heartbeat koşulsuz cevaplanıyordu, yani mimarinin tek 1:1
gelen→giden dönüşümü kimliği doğrulanmış istemcilere sınırsız açıktı.
Aynı mekanizma sınırın ötesine taşındı — yeni bir makine değil, aynı
eşik.

**Neden istemciye görünen semantik değişmiyor:** düzgün bir istemci
saniyede bir heartbeat atar, yani eşiğin kendi temposundadır; her
cevabını ve `HeartbeatAck.tick`'ten okuduğu RTT'yi olduğu gibi alır.
Saniyede binlerce atan bir istemci aralık başına bir cevap alır, gerisi
sayılır.

**Canlılıkla etkileşim (doğrulandı, teste kilitlendi):** idle penceresini
sıfırlayan şey frame'in GELMESİDİR, cevabı değil (`pump.rs` her okumayı
yeniden sarar). Yani cevapsız bırakılan bir heartbeat göndericisini asla
"sessiz" göstermez. Kilit:
`e2e.rs::active_heartbeat_survives_the_idle_window` — eşiğin
cevapladığından dört kat hızlı heartbeat atan istemci
beş saniye boyunca hiç kapatılmıyor, ve ack sayısı İKİ taraftan da
doğrulanıyor (hâlâ cevaplanıyor; artık gönderim hızıyla 1:1 değil).

**Saat tek, sayaç iki.** Saat tek çünkü kısma tek bir hız sınırlayıcıdır;
auth başarısında BİR kez sıfırlanır (§3.3 frame bütçesini emekliye ayıran
aynı faz sınırı), bu da bağlantı ömrü boyunca tam olarak bir fazladan
cevap eder (AUTH bir kez başarılı olur) ve "authenticated oturumun ilk
heartbeat'i her zaman cevaplanır" garantisini verir. Sayaçlar ayrı çünkü
farklı soruları yanıtlarlar: pre-auth fazlalık §3.2'nin GÜVENLİK sinyali
(yanında §3.3 bütçesi ve §4 cap'i vardır, çare bir pre-auth guardrail'i
sıkmaktır), post-auth fazlalık ise BİLİNEN bir istemcinin heartbeat
zamanlayıcısının bozuk olduğunu söyler — bir hata raporu, bir saldırı
değil.

**Bütçeye yazılmıyor** (pre-auth gerekçesinin aynısı): kısma zaten
maliyeti sınırlıyor, dolayısıyla puanlamak düşman tarafında hiçbir şey
kazandırmaz; yalnız dürüst-ama-hatalı istemciyi (gevşek bir NAT
keepalive'ı) zorla düşürür.

## 3.5. Oturum yaşam döngüsü: iki saat, iki yön

Reader pump'un idle penceresi (`idle_timeout_secs`) yarım-açık TCP'yi
yakalar. Soketin DİĞER yarısı bağlantı sınırları turuna kadar sınırsızdı:
okumayı bırakan ama bağlantısını açık tutan bir istemcinin alım penceresi
kapanır, writer pump soket yazmasının içinde süresiz park eder, giden
kanal DOLU kalır (KAPALI değil — yani `w_closing` teardown'ı tetiklenmez),
oda her tick bir frame düşürür ve oturum hiçbir şey alamazken oda
slot'unu ve registry satırını tutmaya devam eder. Gelen sessizlik bunu
göremez, çünkü böyle bir istemcinin sessiz olması gerekmez (ROADMAP'teki
şekil tam olarak "okumayı bırakan ama göndermeye devam eden" istemcidir).

| # | Karar | Gerekçe |
|---|---|---|
| 1 | **`write_stall_secs` (vars. 10 sn; `0` kapatır)** — soketi bu süre boyunca tek bir BAYT bile kabul etmemiş (ve yazılacak bir şeyi olan) bağlantı olağan teardown'la kapatılır | rUDP REL bandının canlılık sınırının TCP tarafındaki kardeşi; `idle_timeout_secs` ile aynı sözleşme (f64 saniye, `0` kapalı) — ikisi bir çifttir, yön başına bir saat |
| 2 | Ölçü **İLERLEME**, yaş değil — ve **BAYT**, kare değil | Yaş düşük-Hz odayı cezalandırır, yüksek-Hz'i ödüllendirir. İlerleme ise tam ayırmak istediğimiz iki istemciyi ayırır: geride kalan ama HÂLÂ BOŞALTAN istemcinin soketi bayt kabul eder ve saat her baytta yeniden başlar — pencereden uzun süren bir KARENİN ortasında da ("yavaş istemci tolere edilir" sözleşmesi aynen durur, düşen snapshot'ları eskisi gibi sayılır); hiç boşaltmayan istemcininki hiç bayt kabul etmez. **Düzeltme (stall gözlemlenebilirliği turu):** ilk uygulama saati yalnız bir karenin gönderimi BÜTÜN OLARAK tamamlanınca sıfırlıyordu; kare pencereden uzun sürede boşalınca okuyan istemci öldürülüyordu — kareler büyüdükçe ilerleme sınırı sessizce bir yaş sınırına dönüşüyordu (10k ölçümü: ~80 KB kareler, 4486 öldürme). Sinyal: yazıcıdaki monoton bayt sayacı (`gsb_net::pump::WriteProgress`), bekleyen işlemin kendi `&mut`'u üzerinden okunur; deadline'da baytlar ilerlemişse AYNI işlem yeni bir pencereyle beklenir |
| 3 | Saat writer pump'un yazma deadline'ında; bağlantı başına zamanlayıcı görev YOK | Reader'ın idiomunun aynısı: tek awaited işlemi bir deadline'a sarmak (`tokio::time::timeout` tek future'a; ikinci canlı kaynak değil) |
| 4 | Verdict aktörün mailbox'ından gider, soketten değil; ve `sink.close()` çağrılmaz | Teardown, tıkanmış olan şeyin bir bayt daha kabul etmesini asla gerektirmemeli (rUDP `die()` ile aynı ilke) |
| 5 | rUDP bu saati almaz | Datagram `try_send_to` park etmez; o yönün canlılığı REL bandının ACK-ilerleme saatidir |
| 6 | Bilinen kalıntılar | (a) TLS: kare tamponu boşaldıktan sonra rustls'in tuttuğu son ≤64 KiB `poll_flush` içinde bayt sayısı raporlamadan boşalır. (b) Çekirdek: dolu gönderim tamponunda bloklu yazıcı ancak tamponun ~üçte biri boşalınca uyandırılır — B baytlık tamponda B / (3 × pencere)'den yavaş okuyan istemci uyanmalar arasında sessiz görünür. (c) WS kapısında soketi başka görev yazar; bayt sayısı pump'a ilk kez deadline'da görünür, orada sınır son bayttan itibaren iki pencereye kadardır |

## 3.6. Sunucu-başlatımlı kapanışların sayımı (stall gözlemlenebilirliği turu)

Bir koruma sessizce kesiyorsa ölçüm yalan söyler: 10k ölçümünde sunucu
4486 oturumu write stall ile kapattı, istemci özeti `errors=0` dedi —
tıkanmış soket ERROR bildirimini taşıyamaz, istemci tarafında hiçbir
sayaç kıpırdamaz. Artık sunucunun KENDİ kararıyla bitirdiği her oturum
sebebiyle sayılır: `gsb_net_server_closes_total{reason=…}` (DESIGN §12).

| Sebep | Koruma |
|---|---|
| `idle_timeout` | §3.5 reader idle penceresi; rUDP demux idle süpürmesi |
| `write_stall` | §3.5 writer ilerleme saati |
| `rel_dead` | rUDP REL canlılık sınırı / retransmit tavanı |
| `violation_budget` | §3 ihlal bütçesi (AUTH seli dahil — §3.1) |
| `preauth_budget` | §3.3 pre-auth kare bütçesi |
| `stream_rejected` | taşıma seviyesi red: `max_frame_bytes`, çözülemeyen kare, WS protokol ihlali, bozuk TLS kaydı (önceden istemci kapanışı gibi görünüyordu) |
| `conn_cap` / `unauth_cap` | `max_connections` / §4 unauthed cap'i, doğumda red |
| `superseded` | aynı kimliğin yeni oturumu eskisini kapattı |
| `room_gone` | oda oturumun altında yok edildi / öldü |
| `outbound_dead` | giden kanal kapalı bulundu ve kayıtlı hüküm yok (çoğunlukla yazma hatasıyla gitmiş bir peer'ın kuyruğu; bekleyen bir stall hükmü ya da peer kapanışı mailbox'tan okunup ona atfedilir) |

Sayılmayanlar, bilerek: istemci-tarafı son (EOF, RST, WS kapanış el
sıkışması) — dökme değildir; sunucu kapanışı (`Shutdown`) — oturum
hakkında hüküm değil ve toplayıcı onunla birlikte öldüğü için
gözlenemez; ticket / protokol sürümü reddi — bağlantı açık kalır;
girdi-boşta tavanı — entity'yi politikaya verir, oturumu bitirmez.

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
Bağlantı sınırları turu: post-auth heartbeat fırtınası → aralık başına
tek cevap + sessiz sayaç + puanlanmama (`security.rs`), auth sınırının
tek sıfırlaması (`security.rs`), kısma ile idle penceresinin dikişi
(`e2e.rs`); hiç okumayan peer → writer pump'un stall raporu ve soketi
beklemeden çıkışı (`gsb-net` `tcp::tests::stall`), ve uçtan uca oturumun
bitişi + registry satırının bırakılışı (`write_stall.rs`).
Stall gözlemlenebilirliği turu: pencereden uzun süren tek bir kareyi
yavaş ama sürekli okuyan peer HAYATTA kalır — TCP (küçültülmüş çekirdek
tamponları), QUIC (istemcinin 2 KiB alım penceresi) ve WS yazma yolu
(`*::tests::slow_reader`); hiç okumayan WS peer'ı hâlâ ölür; her kapanış
yolu kendi sebep kovasında sayılır ve komşu kovalar kıpırdamaz
(`server_closes.rs`, `violation/closes.rs`, `write_stall.rs`).

## 6. NOT-DONE

- mTLS (istemci sertifikası) — ticket-auth yeterli v1'de
- TLS 0-RTT/session resumption ayarları — varsayılanlar
- Admin HTTP auth/TLS — OPS.md NOT-DONE devam
- rUDP crypto — deneysel statü
