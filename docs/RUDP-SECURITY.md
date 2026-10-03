# gsb: rUDP Güvenliği — Noise NK el sıkışması + kendi kayıt katmanımız

> **Durum:** TAMAM — rUDP kripto hattının son turu (B5b) bitti
> (2026-10-03). Bu doküman hattın son başvuru kaynağıdır; dış inceleme
> (D13) kapsamı §11'de devredilmeye hazır. Mühürlü kip sunucunun
> varsayılanı.
> - **x1:** kripto çekirdeği `gsb_net::seal` (sans-IO, soket yok, tokio
>   yok) ve bu doküman.
> - **B3 (2026-10-02):** kriptosuz CID ve göç, kind bayt haritası (§5,
>   B109). §7.
> - **B5a (2026-10-02):** çekirdek rUDP'ye bağlandı — NK msg1 proof'ta,
>   msg2 accept'te, her oturum datagramı iki yönde SEALED, küresel DH
>   bütçesi, göçün üç koşulu, sunucu statik anahtarı config'de, mühürlü
>   kip varsayılan (`udp_security`), düz metin yalnız açık dev/LAN
>   anahtarıyla. Ne yapıldığı: §15; B5b'nin devraldığı: §15 sonu.
> - **B5b (2026-10-03):** anahtar fazı politikası (2 dk / 2^20 kayıt,
>   REL ACK'iyle onay — §6), stateless reset (config'de ya da statik
>   anahtardan türetilen, kapıya bağlı reset anahtarı; tetikleyenden kısa,
>   oranlı reset; istemcide sabit zamanlı jeton kontrolü — §8). CID
>   rotasyonu bu turda yapılmadı: ucuz değil, tasarımı §10. Ne yapıldığı:
>   §16.
> - **Kalan:** CID rotasyonu ve sayaç gizleme (§10, sonra). Dış inceleme
>   (karar 8, D13) kullanıcı kararıyla (2026-10-03) yapılmayacak — proje
>   kendi kullanımımız için; §11 iç gözden geçirme kontrol listesi olarak
>   kalır.
>
> Kaynak araştırma: x1 araştırma raporu (2026-10-02). Bu doküman onun
> §4–§7'sini ve maintainer'ın 10 kararını sözleşmeye çevirir.

## 1. Karar: DTLS kütüphanesi değil, Noise NK + kendi kayıt katmanımız

**Seçilen yol (D):** WireGuard/QUIC biçimli bir düzen.
- **El sıkışma:** Noise `NK_25519_ChaChaPoly_BLAKE2s` (`snow` 0.10, yalnız
  RustCrypto). Bugünkü durumsuz çerez el sıkışmasının proof/accept adımına
  biner: **0 ek RTT**.
- **Kayıt:** `[kind][CID][sayaç]` başlığı + ChaCha20-Poly1305. Başlık
  AEAD'nin ilişkili verisidir (AAD).
- **Göç:** RFC 9146 §6 kuralı: kimliği doğrulanmış + daha yeni + yolu
  doğrulanmış.

**Neden DTLS değil (araştırma §2–§3, doğrulanmış):**

| | DTLS (`dimpl` 0.7.4, en iyi aday) | Noise NK + kendi kayıt |
|---|---|---|
| Saf Rust | `rust-crypto` özelliğiyle evet; varsayılanı aws-lc (C) | Evet (`cargo tree` kanıtı §13) |
| CID (RFC 9146/9147) | **Yok** — göç (B3) için upstream'e yazılmalı | Bizim tasarım (§7) |
| Alma/gönderme bölünmesi | Tek `Dtls` nesnesi ikisini de yapar → demux ile writer arasında paylaşım (kilit ya da mesaj) | `Sealer` / `Opener` ayrı sahipli, paylaşım yok |
| El sıkışma RTT | Çerez + 1 RTT; AUTH'a kadar 3 RTT (bugün 2) | 0 ek RTT |
| Ek yük | DTLS 1.3 + CID ≈ 28 B | c→s 33 B, s→c 25 B |
| C#/Unity | BouncyCastle C#'ta yalnız DTLS 1.2 | BouncyCastle C#'ta X25519 + ChaCha20-Poly1305 + BLAKE2s var → küçük port |
| Bedel | — | Standart uyum yok; kayıt katmanını biz yazıyoruz ve inceletiyoruz (§11) |

**Elenenler:**
- `rtc-dtls`: `ring` zorunlu, yalnız 1.2, CID yok.
- `webrtc-dtls`: async, eski hat.
- `rustls`: DTLS yok.
- `noxtls`: GPL.
- OpenSSL/wolfSSL/mbedTLS: C.

**Fikir kaynakları:**
- WireGuard: kayıt düzeni, ~2000'lik replay penceresi.
- QUIC (RFC 9000/9001): anahtar fazı, bütünlük sınırı, stateless reset,
  yol doğrulaması.
- netcode 1.02: sunucu yeniden başlarken nonce tekrarı tuzağı. Bizde
  yapısal olarak yok: her oturum efemeral DH ile yeni anahtar alır.

## 2. Maintainer kararları (2026-10-02)

| # | Soru | Karar |
|---|---|---|
| 1 | `ring` / C kabul mü? | **Hayır**, bu yolda `ring`, `aws-lc` ve C yok. RustCrypto'nun içerideki SIMD `unsafe`'i kabul (bizim kodumuz değil); bizim kod `unsafe_code = "forbid"` altında |
| 2 | DTLS uyumu gerekli mi? | Hayır. Yalnız kendi istemcilerimiz: Rust + ileride C#/Unity portu |
| 3 | Sunucu kimliği | **Statik X25519 anahtarı.** Platform açık anahtarı bilet ve adresle birlikte verir, istemci sabitler (pin). PKI/sertifika yok. **B21'de uygulandı:** lobinin `gsb_ticket::JoinGrant`'ı `udp_server_key`'i taşır, `gsb-client`'in `grant::connect`'i anahtarı izinden sabitler, anahtarsız izinle udp kapısına gitmez (docs/TICKETS.md §10) |
| 4 | netcode tarzı bilet anahtarı kipi | Şimdi yok |
| 5 | Kriptosuz göç (B3) | **Opt-in.** Kripto gelince varsayılan açık |
| 6 | Kripto gelince düz metin | **Mühürlü (sealed) üretim varsayılanı;** düz metin yalnız açık bir dev/LAN anahtarıyla |
| 7 | CID rotasyonu, sayaç gizleme | Sonra (§10). B5b: CID rotasyonu tasarlandı, yapılmadı (ucuz değil) |
| 8 | Dış güvenlik incelemesi | **Yapılmayacak** (kullanıcı kararı 2026-10-03: proje kendi kullanımımız için); §11 iç kontrol listesi |
| 9 | Yeniden başlatmadan sağ çıkan stateless reset anahtarı config'de | Evet (§8). B5b: `udp_reset_key` opsiyonel; yazılmazsa statik anahtardan türetilir (o da config'de ve kalıcı) |
| 10 | Göçte yeni adrese erken gönderim mi? | Hayır: **yeni yol doğrulanana kadar eski yolda beklenir** (§7) |

## 3. Tehdit modeli

**Saldırgan türleri:**
- **Yol dışı:** paketleri göremez, adres sahteler.
- **Koklayıcı:** paketleri görür, enjekte eder.
- **Yol üstü:** düşürür, geciktirir, değiştirir.

| Saldırı | Düz metin kapı | B3 (kriptosuz CID, opt-in) | Mühürlü kapı (B5a + B5b) |
|---|---|---|---|
| Kare okuma | Koklayıcı her şeyi okur | Aynı | Şifreli. Görünen: CID (c→s), sayaç, boy, zamanlama, anahtar fazı biti — **mühürlü kapıda geçerli** (bu sütunun tamamı) |
| Kare enjeksiyonu / değiştirme | Koklayıcı enjekte eder; yol dışı, portu bilirse sahte RAW/FRAG basar | Aynı | AEAD reddeder → `seal_forged` |
| Tekrar oynatma | Mümkün | Mümkün | Replay penceresi → `seal_replayed` / `seal_too_old` |
| Oturum kaçırma | Adres sahteciliğiyle kısmen | **CID taşıyıcı jetondur:** CID'yi koklayan, düz metin PATH_CHALLENGE'ı kendi adresinden yanıtlayıp s→c akışını kendine çeker | Challenge şifreli, yanıtlanamaz; eski yolda beklenir |
| Yansıtma / amplifikasyon | Çerez + oran ≤ 1 | + 3x bütçe + yol doğrulaması | Aynı. Accept (77 B) proof'tan (~67 B) büyük, ama yalnız çerezle kanıtlanmış adrese gider. Stateless reset tetikleyenden kesin kısa (≤ 41 B) ve oranlı (B5b, §8) |
| DH seli (CPU) | — | — | DH yalnız çerez doğrulandıktan ve kaynak başına bekleyen oturum sınırından (B89, yapıldı) **sonra**; küresel DH bütçesi B5a'da (§4) |
| Sahte sunucu | Mümkün | Mümkün | İstemci sunucu açık anahtarını sabitler; NK msg2'yi yalnız gerçek sunucu üretebilir |
| Sunucu yeniden başlarsa | İstemci 5 sn REL sınırını bekler | Aynı | Stateless reset jetonuyla hemen biter (B5b, yapıldı; aynı adrese bağlanan sunucu) |
| Sahte reset (oturum bitirme) | — | — | Jeton yalnız istemci ve sunucuda (msg2'de şifreli); sabit zamanlı karşılaştırma; bir kapı başka bir kapının canlı oturumunun jetonunu vermez (§8) |
| Ele geçen anahtarın geçmişi açması | — | — | Faz değişiminden önceki trafik kapalı kalır (REKEY tek yönlü; faz 2 dk / 2^20 kayıt, §6). Efemeral DH oturumlar arası ileri gizliliği zaten verir |
| Pasif bağlanabilirlik (gizlilik) | Adres | CID ağlar arası sabit | Aynı. Çözüm CID rotasyonu + sayaç gizleme (§10.1 tasarım, sonra) |

**Kapsam dışı:**
- Uç noktaların ele geçirilmesi.
- Sunucu statik anahtarının sızması. Geçmiş oturumlar efemeral DH
  sayesinde korunur (ileri gizlilik), ama sızan anahtarla sunucu taklit
  edilebilir. Çözüm: platform yeni açık anahtarı dağıtır.
- Trafik analizi.

## 4. El sıkışma akışı (bugünkü çerez el sıkışmasının üstünde, 0 ek RTT)

```text
istemci                                          sunucu
HELLO{n,0}                          18 B  ──▶
                                          ◀──  HELLO{n,cookie}   18 B   durum yok, DH yok
proof{n,cookie} + caps + msg1       ~67 B ──▶  1) Msg1::parse   (yalnız uzunluk)
  msg1 = e(32) + şifreli yük + tag(16)          2) çerez doğrulaması (bugünkü kod)
                                                3) Msg1::cookie_verified → DH burada
                                          ◀──  ACK{1} + msg2     5 + 72 B
                                                 msg2 = e(32) + {cid u64, reset jetonu 16} + tag(16)
SEALED REL AUTH(bilet)              ──▶        AUTH artık şifreli kanalın içinde
```

**Kurallar:**
- **DH sırası API'de açık.** `Msg1::parse` yalnız boy kontrol eder
  (48..=80 B). X25519 işlemleri yalnız `Msg1::cookie_verified` içinde
  koşar. Çağıran oraya ancak çerez kontrolünden sonra gelir.
  - Sunucu el sıkışma başına 3 skaler çarpım yapar: `es`, efemeral üretimi,
    `ee`. snow statik açık anahtarı her responder kurulumunda yeniden
    türettiği için +1 eder.
  - **Ölçüldü (B110, u89; `seal::tests::cost`, yok sayılan zamanlama
    sondası, release):** Ryzen 9 7950X, **yüklü** makine (yük ortalaması
    ~36 / 32 iş parçacığı), 2000'lik üç koşu: responder'ın bütün
    `Msg1::cookie_verified`'ı **175–186 µs**, istemcinin msg1'i 85–90 µs,
    bir X25519 (anahtar çifti üretimi, OS entropi okuması dahil) 55–58
    µs; oran 3,1–3,2 (üretimin entropi okuması payı yüzünden 4'ün
    altında). Çekirdek-saniye başına **~5,4–5,7 bin el sıkışma**. Sessiz
    makinede daha düşük beklenir; B5a sessizde yeniden ölçer.
  - **Sonuç:** demux tek görev; DH orada koşarsa her el sıkışma bütün
    oturumların gelen trafiğini ~180 µs durdurur, saniyede ~5,5 bin
    doğrulanmış proof demux'ı doyurur. B89'un kaynak başına sınırı
    eşzamanlı kuruluşları keser, **hızı** kesmez (SECURITY §4.3.3 #5).
    Statik anahtarın yeniden türetilmesi (+1 çarpım) el sıkışmanın ~%25'i:
    `clatter` ya da kendi NK'mız onu geri alır (B110 açık kalır, B5a'nın
    ölçümüne bağlı).
  - **B5a'nın DH bütçesi — YAPILDI (B119 kapandı; `udp::sealed::budget`).**
    Kodlanan: aşağıdaki 1. madde; kova 50 ms'lik (oranın 1/20'si, en az
    bir jeton): en uzun art arda DH koşusu ~9 ms (REL'in 50 ms
    tabanının altında). Sıra: çerez → msg1 boyu → kaynak sınırı → kova
    → CID → DH; mutasyonla kilitli (DH kovadan önce; kova kaynak
    sınırından önce). Tokio saati: duraklatılmış saatli testler kovayı
    sürer. 2. ve 3. madde B120/B121 olarak açık (B5a ölçümü, §15).
    Asıl tasarım metni:
    1. Küresel jeton kovası, demux'ta, çerez ve kaynak sınırından
       sonra, DH'den önce: `udp_handshakes_per_sec` (sunucu anahtarı;
       vars. demux'ın bir çekirdeğinin ~%20'si — ölçülen değerle ~1000/s;
       `0` = sınırsız). Kova boşken doğrulanan proof hiçbir şey kurmaz,
       kabul almaz, kendi adıyla sayılır (`udp_proofs_refused_budget`);
       istemci yeniden gönderir (5 sn içinde). Durum: iki sayı (jeton,
       son dolum anı), saat okuması proof başına.
    2. Kovanın payı: küresel kova demux'ı korur, adaleti değil — tek
       (dönüş yolu olan) kaynak art arda proof'la kovayı tüketebilir ve
       dürüst kaynakların el sıkışması yeniden gönderime kalır. B89'un
       sınırı eşzamanlılığı keser, hızı kesmez; D12 kaydı reddeder ama
       DH'den sonra. Kaynak başına oran (küçük kova; tablo yalnız son
       kaynaklar için, sınırlı) B5a ölçümüne bağlı ikinci adım.
    3. Seçenek (ölçüm gösterirse): DH'yi demux'tan sınırlı bir işçi
       havuzuna taşımak (kuyruk = bütçe), demux yalnız sıraya koyar.
       Kuyruk doluysa aynı ret yolu.
- **Prologue = `"gsb-rudp-seal/1\0"` + bağlam.** B5a bağlam olarak HELLO
  nonce'u ve çerezi verir. Böylece Noise dökümü o çerez alışverişine
  bağlanır: başka bir çerezle yakalanmış msg1 tutmaz. Bağlam uyuşmazlığı
  `HandshakeError::Decrypt` olur.
- **msg1 yükü:** en çok 32 B (`MSG1_PAYLOAD_MAX`), ör. yetenek bitleri.
  - NK msg1 yükü sunucu anahtarına şifrelidir ama **tekrar oynatılabilir**
    ve ileri gizliliği yoktur.
  - Bu yüzden **bilet asla msg1'de gitmez.** Bilet, el sıkışmadan sonra
    SEALED REL içinde AUTH olarak gider.
- **msg2 yükü sabit 24 B (`Accept`):** tel CID'si (rastgele, sunucu seçer;
  `gsb_core::ConnectionId` değil) + stateless reset jetonu.
- **İdempotent proof:** sunucu oturum başına msg2 baytlarını saklar. Aynı
  proof yeniden gelirse aynı baytları yeniden gönderir; ikinci DH yok.
  İstemci proof'u yeniden gönderirken `Initiator::msg1()`'in aynı
  baytlarını kullanır.
- **Sahte accept el sıkışmayı öldürmez.** `Initiator::finish` başarısız
  olursa snow durumu geri alınır; istemci gerçek msg2'yi beklemeye devam
  eder (testli).
- **İstemci kimliği:** NK katmanında istemci anonimdir; kimliğini şifreli
  AUTH biletiyle kanıtlar. `Session::handshake_hash()` kanal bağlama için
  açıktır: platform bileti ileride bu hash'e bağlayabilir (opsiyonel).
- **Sunucu statik anahtarı config'de** (B5a, yapıldı): `udp_static_key`
  (64 hex) ya da `udp_static_key_file`. Yoksa, ikisi birden varsa ya da
  bozuksa başlatma hatası olur; sessiz düz metin geri düşüşü yoktur
  (SECURITY §2 karar 3'ün ilkesi). Biçim gerekçesi §15.
- **Hata adları (`HandshakeError`):** `Malformed` (DH'den önce), `Decrypt`,
  `PayloadTooLarge`, `Internal`. Her biri ayrı sayılır.

**Neden BLAKE2s (SHA-256 değil):**
- WireGuard'ın seçimi.
- SHA uzantısı olmayan 32/64 bit CPU'larda (mobil, Unity) yazılımda hızlı.
- snow çözücüsünde `ring`'siz var.
- C# tarafında BouncyCastle'da var.
- Reset jetonu da aynı hash'i kullanır: protokolde tek hash fonksiyonu.

## 5. SEALED kayıt düzeni

```text
c→s  [kind 1][cid u64 LE 8][sayaç u64 LE 8][şifreli iç datagram][tag 16]   ek yük 33 B
s→c  [kind 1]              [sayaç u64 LE 8][şifreli iç datagram][tag 16]   ek yük 25 B
kind = 0x40 | anahtar fazı biti (0x01)
```

- **İç datagram, bugünkü datagramın aynısıdır** (RAW/REL/ACK/FRAG/PROBE/
  REPORT ve B3'ün PATH_*'ı). Anlamları içeride değişmez.
  - B5a bütçeyi ek yük kadar küçültür: 1472 − 33 c→s, 1472 − 25 s→c.
- **kind 0x40 — kesin (B3, B109 kapandı).** Kind bayt haritası:

  | Aralık | Anlamı |
  |---|---|
  | `0x00..=0x3F` | Düz metin türler: `0` RAW, `1` REL, `2` ACK, `3` HELLO, `4` FRAG, `5` PROBE, `6` REPORT, `7` PATH_CHALLENGE, `8` PATH_RESPONSE; `9..=0x3F` boş |
  | `0x40..=0x7F` | SEALED kayıt: `0x40 \| faz` (`0x40`, `0x41`); `0x42..=0x7F` boş. s→c stateless reset de bu biçimi taşır (§8, B5b) |
  | `0x80..=0xBF` | CID etiketli düz metin tür `0x80 \| k` (yalnız c→s, B3): `[0x80\|k][u64 cid][k'nın gövdesi]` |
  | `0xC0..=0xFF` | Boş (SEALED etiket bitini almaz: CID'yi kendi başlığında taşır) |

  - Etiketli düz metin ile SEALED c→s, CID'yi **aynı konumda** taşır
    (bayt 1..9): demux iki biçimde de oturumu aynı yerden bulur.
  - Ayrıklık ve konum eşitliği derleme zamanı `assert`'leridir
    (`crates/gsb-net/src/udp/mod.rs`).
  - Eski sunucu bilmediği türü düşürür ve `udp_datagrams_malformed`'a
    sayar (`_ => bad_datagrams` kolu, oturuma dokunmaz); yeni istemci
    zaten yalnız CID verilmişse etiketler.
- **AAD = başlığın kendisi.** `Sealer::seal` başlığı kendisi yazar ve AAD
  olarak kullanır; başlık ile nonce ayrışamaz.
- **Nonce:** Noise ChaChaPoly kodlaması: 4 sıfır bayt + sayaç (u64 LE).
  Yön başına ayrı anahtar: Noise `Split()`; ilk anahtar istemci → sunucu.
- **Sayaç sınırı `SEAL_LIMIT` = 2^62:** QUIC paket numarası tavanı. Noise
  2^64−1'i REKEY'e ayırır. Saniyede 1M datagram ile ~146 bin yıl.
  - Önemli olan: sarma yok, sert ret var (`SealError::CounterExhausted`).
  - Açıcı bu sınırı aşan sayacı AEAD'ye hiç sokmaz (`seal_malformed`).
- **Replay penceresi `REPLAY_WINDOW` = 1024** (RFC 6479 biçimli bit halkası).
  - Oturum başına 128 B; 100k oturumda 12,8 MB. WireGuard'ın ~2048'i 25,6 MB
    olurdu.
  - Oyun oturumu saniyede onlarca–birkaç yüz datagram gönderir. 1024 sayaç
    saniyelerce sırasızlık demektir; daha geç gelen oyun için zaten bayattır
    ve REL bandı onu yeniden gönderir.
  - Pencere içinde sırasız gelen kabul edilir; tekrar ve çok eski reddedilir.
  - Pencere **yalnız doğrulanmış** datagramla ilerler: sahteci gelmemiş
    yuvaları yakamaz (mutasyonla doğrulandı).
- **Bütünlük sınırı `INTEGRITY_LIMIT` = 2^36** (RFC 9001 §6.6,
  AEAD_CHACHA20_POLY1305). Oturum ömrü boyunca, tüm anahtarlar üzerinden
  sayılır. Aşılınca her açma `seal_integrity_limit` ile reddedilir ve
  oturum kapatılmalıdır.

**Açıcının retleri ("her kaybı say").** Sıra en ucuzdan pahalıya; ilk
tutan ad datagramın adıdır, tek datagram tek ad alır:

| Sıra | `Refusal` | Sayaç adı | Anlamı |
|---|---|---|---|
| 1 | `IntegrityLimit` | `seal_integrity_limit` | Oturum 2^36 sahte sınırını aştı; hiçbir şey açılmaz |
| 2 | `Malformed` | `seal_malformed` | SEALED değil, başlık + tag'e yetmez, ya da sayaç ≥ 2^62 |
| 3 | `TooOld` | `seal_too_old` | Pencerenin altında (gerçek olabilir) |
| 4 | `Replayed` | `seal_replayed` | Bu sayaç zaten açıldı |
| 5 | `WrongPhase` | `seal_wrong_phase` | Faz biti sayaçla çelişiyor: mevcut fazda açılmış bir sayacın altında yeni faz. Dürüst eş göndermez; AEAD'ye girmez |
| 6 | `Forged` | `seal_forged` | Kimlik doğrulama başarısız (sahte, bozuk, yanlış anahtar) |

- `Opened { plaintext, counter, newest }`: `newest` = şimdiye kadarki en
  yüksek sayaç. Göç kuralının ikinci koşuludur (§7).
- 3–5 AEAD'den önce elenir. Bu yüzden sayaçtaki tekrarı taşıyan sahte
  datagram `Forged` değil `Replayed` sayılır. Bilinçli: ucuz kontrol önce.

## 6. Anahtar fazı ve yeniden anahtarlama

**Sonraki anahtar:** Noise `REKEY(k) = ENCRYPT(k, 2^64−1, "", sıfır(32))[..32]`.
- Tek ChaCha20 bloğu, yeni bağımlılık yok.
- Tek yönlü: eski anahtar yenisinden hesaplanamaz.
- snow'un `rekey_*`'ıyla bayt bayt aynı (test). C# portu aynı vektörlerle
  sınanır.
- HKDF yerine bu seçildi: ek crate gerekmez ve Noise spesifikasyonunun
  tanımlı işlemi.

**Faz biti:** kind'in 0x01 biti = anahtar neslinin düşük biti. Sayaç
nesiller boyunca sürer (QUIC paket numarası gibi). Böylece tek bir replay
penceresi tüm fazları kapsar ve nonce hiçbir anahtar altında tekrar etmez.

**Gönderen kuralları (`Sealer::rekey`):**
1. `RekeyTooSoon`: önceki rekey'den bu yana en az `REKEY_MIN_DISTANCE`
   (= pencere, 1024) datagram mühürlenmiş olmalı.
2. `RekeyUnconfirmed`: eş mevcut fazdan bir datagramı onaylamış olmalı
   (`note_peer_ack`; RFC 9001 §6.1'in kuralı).
   - REL katmanı ACK'i gönderdiği sayaca eşler (B5b, aşağıda "Politika").

**Alıcı (`Opener`):**
- Mevcut anahtar, önceden hesaplanmış sonraki anahtar ve **bir** önceki
  anahtarı tutar.
- Faz biti çevrilmiş ve sayaç şimdiye kadarkilerin hepsinden büyükse
  sonraki anahtar denenir; doğrulanırsa terfi eder.
- Faz biti eski ve sayaç yeni fazın başlangıcının altındaysa önceki
  anahtar kullanılır (sırasız gelenlere tolerans, "grace").
- Önceki anahtar, pencere faz sınırını geçince (`top ≥ başlangıç + 1023`)
  bırakılır. O andan sonra eski fazın her sayacı zaten `TooOld`'dur.
- İki gönderen kuralı birlikte şunu garanti eder: dürüst bir eşin
  datagramı, alıcının elinde anahtarı olmayan bir nesilden asla gelmez.
  Seeded model testi bunu 3 tohum × 20k adımda doğrular.

**Politika (B5b, yapıldı — `udp::sealed::rekey`):** her mühürlü gönderme
yarısı (yazıcının s→c'si, istemcinin c→s'si) bir `SendHalf`'tır; iki yön
bağımsız rekey eder.
- **Tetik:** faz **`RekeyPolicy::after` = 2 dk** ya da
  **`after_records` = 2^20 kayıt** sonra biter, hangisi önce gelirse
  (`DEFAULT_REKEY_AFTER`, `DEFAULT_REKEY_AFTER_RECORDS`;
  `UdpTransportConfig::rekey`, `UdpClientConfig::rekey`; sunucu config
  anahtarı yok). Mühürlemeden hemen önce bakılır; trafiği olmayan oturum
  rekey etmez (korunacak bir şey yok).
- **Sayıların gerekçesi:**
  - 2^62 sayaç tavanı fazla ilgisizdir: sayaç fazlar boyunca sürer,
    tavan oturumundur (saniyede 1M kayıtla ~146 bin yıl).
  - 2^36 bütünlük sınırı tüm anahtarlar üzerinden SAHTELERİ sayar (RFC
    9001 §6.6): rekey onu sıfırlamaz, ona yaklaşmayı da değiştirmez.
  - ChaCha20-Poly1305'in 2^62'nin altında pratik gizlilik sınırı yoktur
    (RFC 9001 §6.6).
  - Yani rekey zorunluluk değil, **ele geçen bir anahtarın açtığını
    daraltmaktır**: REKEY tek yönlüdür, k_n'den k_{n−1} hesaplanamaz —
    bir bellek dökümü yalnız mevcut ve SONRAKİ fazları açar, öncekileri
    değil. 2 dk WireGuard'ın REKEY_AFTER_TIME'ıdır (orada tam DH; bizde
    simetrik zincir — DH'li ileri gizlilik oturum başına efemeral
    anahtardan gelir).
  - Oyun hızları: 20–60 Hz snapshot + girdi + ACK ≈ saniyede onlarca–
    birkaç yüz kayıt → 2 dk'da ~5–50 bin kayıt; süre önce dolar. 2^20
    kayıt yalnız saniyede ~8,7 binden hızlı (toplu) gönderende önce
    dolar; o fazı 2^20 kayıtla sınırlar. 60 Hz bir oturum 2^20'ye ~4,8
    saatte varır.
  - Asgari mesafe (1024 kayıt) yavaş oturumu sınırlar: saniyede 5 kayıtla
    faz ≥ ~3,4 dk olur. Bu erteleme normaldir, sayılmaz.
- **Onay (ACK → sayaç eşlemesi):** her REL çerçevesinin **ilk**
  gönderiminin kayıt sayacı `(seq, sayaç)` olarak tutulur; yeniden
  gönderim yeni sayaç alır ama eşlemeye girmez — ACK herhangi bir kopyaya
  cevap olabilir, güvenle kefil olunabilen yalnız en küçük sayaçlı
  ilkidir (ilk kopya mevcut fazdaysa bütün kopyalar da öyledir). Birikimli
  ACK `a` geldiğinde `seq < a` olan her girdi `note_peer_ack(sayaç)`'a
  gider ve bırakılır (`seq = a` henüz kapsanmadı). Sınır: `RETRANSIT_CAP`
  (bandın kendi çerçeveleriyle birlikte büyür ve küçülür). Canlı oturumda
  heartbeat (REL) iki yönde de bu kanıtı üretir.
- **Hiç onaylamayan eş:** hiçbir şey durmaz — oturum mevcut anahtarla
  mühürlemeye devam eder; vadesi gelen ama onaysız her deneme 10 sn'de
  (`REKEY_RETRY`) en çok bir kez sayılır (`udp_rekeys_unconfirmed`,
  istemcide `rekeys_unconfirmed`); ilk onaydan sonraki ilk kayıtta rekey
  (`udp_rekeys`, `rekeys`).
- **Testler** (`udp::sealed::tests::rekey`, elle sürülen saat): süre ve
  kayıt tetiği, mesafe kuralı (fazlar tam 1024'er), hiç onaylamayan eş
  (60 sn'de 6 sayım, oturum sürer, onayla hemen rekey), ACK'in neyi
  kanıtladığı (ilk gönderim, birikimli noktanın altı), politika sınırında
  sırasızlık (önceki anahtar toleransı). Kablolu: `udp::writer::tests::
  seal`, `udp::client::tests::seal`. Mutasyonla kilitli: onaysız rekey,
  `seq > a`, faz denetimsiz `note_peer_ack`, yazıcının/istemcinin ilk
  gönderimi kaydetmemesi, mesafe kuralı, sayımın kısılmaması.

## 7. CID ve göç kuralları (B3 + B5a)

**CID kuralları:**
- **Her c→s datagramı CID taşır.** İstemci kendi NAT yeniden bağlanmasını
  göremez.
- CID 64 bit, rastgeledir (`getrandom`) ve sunucu seçer. Kriptodan önce
  B3 onu accept'te düz verir; kriptodan sonra msg2'nin şifreli yükünde
  verilir.

**Adres güncelleme kuralı (RFC 9146 §6 biçimi).** Sunucu oturumun
adresini yalnız şu üçü birden doğruysa değiştirir:
1. **Kimliği doğrulanmış:** datagram `Opener::open`'dan `Ok` döndü.
2. **Daha yeni:** `Opened::newest`, yani sayaç şimdiye kadarki en yüksek.
   Eski bir datagramı başka adresten yeniden oynatmak göç tetiklemez.
3. **Yolu doğrulanmış:** yeni adrese şifreli `PATH_CHALLENGE` (8 B
   rastgele) gider; aynı değeri taşıyan şifreli `PATH_RESPONSE` o
   adresten döner. Kripto sonrası PATH_* iç kind'lerdir.

**Bekleme ve bütçe:**
- **Karar 10:** doğrulanana kadar s→c akışı **eski adreste kalır.**
  Bedeli ~1 RTT oyun bandı kaybıdır; REL zaten yeniden gönderir.
- Doğrulanmamış adrese en çok 3x bayt gider (yalnız challenge).
- RFC 9853'ün "enhanced" kipiyle eski adres de yoklanır. Taze datagramı
  başka adresle yarıştıran saldırgan challenge'ı çözemez.

**Kriptosuz göç (B3):**
- **Opt-in** (karar 5). Çünkü orada CID taşıyıcı jetondur (§3).
- Kripto gelince (B5a) varsayılan açılır.

**B3'te yapılan (2026-10-02; ayrıntı ve gerekçeler DESIGN §6
"Bağlantı göçü", kod `crates/gsb-net/src/udp/path.rs`):**
- **Config:** `udp_migration = true|false`, varsayılan `false` (kapı
  bayt bayt eskisi — testle kilitli).
- **CID:** 64 bit, `getrandom` 0.4.3, oturum başına; entropi
  başarısızsa oturum CID'siz kurulur ve sayılır, zayıf değer yok.
- **Tel (eklemeli):**
  - proof `[3][nonce][cookie][0][u8 caps]` (19 B; caps bit 0 = CID);
  - accept `[2][u32 1][u64 cid]` (13 B; yeniden proof'a aynı CID);
  - etiketli c→s `[k|0x80][u64 cid][gövde]`;
  - `PATH_CHALLENGE [7][u64 nonce]` (s→c, 9 B);
  - `PATH_RESPONSE [0x88][u64 cid][u64 nonce]` (c→s, 17 B).
- **Kurallar:** yukarıdaki üç koşulun kriptosuz karşılığı yalnız 3.'sü
  (yol doğrulaması) — 1. (kimlik) ve 2. (`newest`) B5a'nın. Doğrulanana
  kadar s→c eski yolda (karar 10); doğrulanmamış adresten GELEN girdi
  oturuma kabul edilir (RFC 9000 §9; kriptosuz bu, eski adresi
  sahtelemekten fazlasını açmaz); challenge en çok 200 ms'de bir ve
  yalnız aday konuştukça, ≤ 3× bütçe; doğrulama 3 sn'de zaman aşımına
  uğrar; üçüncü adres bekleyeni değiştirir (en yeni aday — B5a'da
  `newest` ile gerçek sıra); başka oturumun adresi aday olamaz.
- **Yol tahmini:** yeni IP'de RTT, oyun bandı tahmini ve tıkanıklık
  sıfırlanır (RFC 9000 §9.4); yalnız port değişimi korur.
- **Sayaçlar:** `udp_cids_assigned`, `udp_entropy_draws_failed`,
  `udp_cid_unknown`, `udp_path_validations_{started,timed_out,
  superseded,open_at_end}`, `udp_path_challenges_{sent,send_failed}`,
  `udp_path_amplification_capped`, `udp_path_address_in_use`,
  `udp_path_responses_unmatched`, `udp_path_changes_not_forwarded`,
  `udp_migrations`, `udp_migrations_port_only` (OPS §3).
- **Amplifikasyon notu:** bugünkü boylarla 3× sınırı bağlamaz (aday
  ≥ 9 B'lik datagram'la doğar, challenge 9 B, yeniden gönderim yalnız
  adaydan yeni datagram'la); kural ve sayacı B5a'nın mühürlü boyları
  için yerinde ve testli.

**B5a'nın B3'ten devraldığı — YAPILDI (2026-10-02; ayrıntı §15):**
- CID accept'in düz metin uzantısı yerine msg2'nin şifreli yükünde
  (`Accept`); mühürlü kapıda HER oturuma verilir (kayıtların
  yönlendirme anahtarı); caps baytı msg1'den önce kaldı ve mühürlü
  proof'ta hep var (msg1 bayt 19'da).
- Etiketli düz metin yerine SEALED c→s (CID aynı bayt 1..9'da); PATH_*
  şifreli iç tür (`PATH_RESPONSE` artık etiketsiz `[8][u64]`); demux'ın
  yönlendirmesi CID'le.
- Göç kuralının 1. ve 2. koşulu: başka adresten gelen kayıt yalnız
  `Opener::open` Ok + `Opened::newest` ise aday olur; açılan ama en yeni
  olmayan işlenir, göç başlatmaz (`udp_path_candidates_not_newest`).
  "En yeni aday" kuralı artık sayaç sırasıyla. Challenge yazıcı
  üzerinden mühürlü gider (`UDP_SEND`); 3× bütçe mühürlü boyla
  (9 + 25 B) hesaplanır.
- `udp_migration` varsayılanı mühürlü kapıda açık (B112 kapandı); düz
  metin kapıda kapalı kaldı (opsiyonel `bool`: yazılmamışsa kipe göre).
- Bildirim yolu (B89/B113) aynı kaldı.

## 8. Stateless reset (B5b, yapıldı)

**Anahtar (karar 9):**
- Sunucu config'inde 32 B'lik reset anahtarı: `udp_reset_key` (64 hex)
  ya da `udp_reset_key_file` — **opsiyonel**. Yazılmazsa
  `ResetKey::derived_from(statik anahtar)` =
  `HMAC-BLAKE2s(statik özel anahtar, "gsb-rudp-reset-key/1")`.
  - **Neden zorunlu değil (taslaktaki "yoksa başlatma hatası" yerine):**
    statik anahtar mühürlü kapıda zaten zorunlu ve kalıcı (istemciler onu
    sabitler; değişirse herkes yeni açık anahtar alır). Ondan etiketli bir
    PRF ile türetilen anahtar her yeniden başlatmadan sağ çıkar, ikinci
    bir sır yönetmeyi gerektirmez; yeni bir zorunlu config anahtarı ise
    her dağıtımı kırardı. Türetilen anahtar statik anahtar hakkında bir şey
    söylemez (HMAC çıktısı) ve DH onu hiç görmez. Ayrı anahtar yalnız
    ikisini ayrı döndürmek için (sızan reset anahtarı = o kapının
    oturumlarını bitirebilme; statik anahtarı değiştirmeden döndürülür).
  - **Kapı başına rastgele anahtar (B5a'nın geçici hâli) reddedildi:**
    yeniden başlatmada kaybolur — tam da resetin işe yaraması gereken
    an.
- **Kapıya bağlama:** her kapı anahtarı bağlı adresine bağlar:
  `for_door(adres) = HMAC-BLAKE2s(anahtar, "gsb-rudp-reset-door/1" ‖
  adres)`. Aynı kapı yeniden başlayınca aynı anahtarı türetir; iki kapı
  birbirinin jetonunu ASLA vermez. Bağlamasaydık: bir kapıdaki canlı
  oturumun CID'sini koklayan, CID'yi bilmeyen ÖBÜR kapıya bir datagram
  yollayıp jetonu alır, kurbanın oturumunu bitirirdi. Bedeli: yeniden
  başlayan sunucu AYNI `bind` adresine bağlanmalı (OPS §2).
- Jeton: `HMAC-BLAKE2s(kapı anahtarı, "gsb-rudp-reset/1" ‖ cid_le)[..16]`
  (`ResetKey::token`). Accept'te (msg2) şifreli gider; yalnız istemci ve
  sunucu bilir.

**Reset datagramı (`seal::reset_datagram`):**

```text
[0x40 | rastgele faz biti][rastgele sayaç < 2^62, u64 LE][rastgele dolgu][jeton 16]
boy = min(tetikleyen − 1, RESET_LEN_MAX = 41), en az RESET_LEN_MIN = 26
```

- **SEALED s→c kaydı biçiminde** (RFC 9000 §10.3'ün fikri): kind
  baytı, sayaç boyunda bir alan, şifreli metin yerinde rastgele bayt,
  tag yerinde jeton. Sayaç 2^62'nin altında tutulur: istemcinin açıcısı
  onu `Malformed` değil AEAD'de `Forged` olarak reddeder (testli).
  Boy aralığı küçük kontrol kayıtlarınınki (ACK 30 B, PROBE /
  PATH_CHALLENGE 34 B).
- **Ayırt edilemezlik sınırı (bilinçli):** sayaç düz gider (§10'un sayaç
  gizlemesi sonra); oturumun sayaçlarını izleyen gözlemci rastgele bir
  sayaç görür. Bayt biçimi ve boy açısından küçük bir kayıttır.
- **Amplifikasyon yok:** reset tetikleyenden KESİN kısadır, en çok 41 B,
  yalnız tetikleyenin kaynağına gider. Tetikleyen c→s kaydı en az 33 B
  (başlık + tag) olmalı ki CID okunabilsin; daha kısası `seal_malformed`.
  Döngü yok: iki uç birbirinin datagramını bilinmeyen oturum sanırsa her
  tur bir bayt kısalır, 33 B'nin altında durur (≤ 9 tur; istemci zaten
  reset göndermez).
- **Oran (`udp_stateless_resets_per_sec`, vars. 10 000/s, `0` = yok):**
  kapı başına jeton kovası (DH bütçesinin `DhBudget`'ı, 50 ms'lik), entropi
  çekiminden ve HMAC'ten ÖNCE. Aşan tetikleyici cevapsız kalır, sayılır
  (`udp_stateless_resets_rate_limited`). Gerekçe: reset başına ~1 HMAC
  (~4 BLAKE2s sıkıştırması) + bir `sendto` ≈ ~3 µs (tahmin, ölçülmedi)
  → 10 000/s ≈ demux çekirdeğinin %3'ü; yeniden başlayan sunucu N oturumu
  ~N / oran saniyede sıfırlar (her istemci kovada jeton bulan ilk
  datagramıyla) — 10 000 oturum ~1 sn, 5 sn REL sınırının altında.
- **Sayaçlar:** `udp_stateless_resets_sent`, `_rate_limited`,
  `_send_failed`; tetikleyen datagram her durumda `udp_cid_unknown`
  (datagramın adı; reset bir sonuçtur).

**İstemci (`udp::client::seal`):**
1. Açıcı datagramı reddeder (bütünlük sınırı dışındaki herhangi bir
   retle — reset hemen her zaman `Forged` olur, ama `TooOld` /
   `Replayed` / `WrongPhase`'e düşen bir reset de kaçmasın diye).
2. Datagram SEALED türlü ve reset boyundaysa (26..=41 B) son 16 baytı
   msg2'nin jetonuyla **sabit zamanlı** karşılaştırılır
   (`ResetToken::matches`, `subtle`).
3. Tutarsa oturum hemen biter (`is_established()` → `false`,
   `ended()` → `UdpEnd::Reset`, okuma `Recv::Closed` — B128; dışarıda
   kalan REL çerçeveleri `gave_up`) ve datagram **yalnız**
   `stateless_resets_received` sayılır (§8'in eski açık sorusu: tek ad —
   `seal_forged`'a girmez). Çağıran resume yoluna geçer (yeni soket, yeni
   el sıkışma, aynı adla AUTH — RECONNECT §5).
4. Tutmazsa datagram kendi `seal_*` adıyla sayılır, ayrıca
   `stateless_resets_invalid` ("bunlardan": reset boyunda olup jetonu
   tutmayan — anahtarı ya da adresi değişmiş bir sunucunun reseti ile
   küçük sahte kayıt bilerek ayırt edilemez).

**Testler:** `seal::tests::reset` (türetmeler; 0..=1472 her tetikleyen
boyu için kısa reset; düzen; açıcının reddi ve kuyruk), `udp::demux::
tests::sealed::reset` ("yeniden başlamış" demux: jeton, boy sınırı, oran,
kapalı), `udp::sealed::tests::door` (kapıya bağlama, yapılandırılmış
anahtar, oran ve politika config'ten), `udp::client::tests::reset`
(doğru jeton bitirir, yanlış jeton / reset olamayacak biçim bitirmez),
`udp::tests::reset` (gerçek soket: kapı kapanır, aynı adrese yenisi,
istemci < 1 sn'de biter, yeni oturum kurar), `gsb-server`
`tests/rudp_resume.rs` (iki sunucu örneği aynı anahtarlarla: reset,
< 1 sn, aynı adla yeniden katılma). Mutasyonla kilitli: yanlış jetonla
kabul, tetikleyenden büyük/sınırsız reset, oransız reset, demux'ın reset
göndermemesi, kapıya bağlamama.

## 9. Düz metin kipi

B5a'dan beri **mühürlü kip üretim varsayılanıdır** (karar 6). Düz metin
rUDP yalnız açık bir config anahtarıyla açılır (yapıldı):
- `udp_security = "plaintext"` (vars. `"sealed"`), "dev/LAN" diye
  belgeli (OPS §2, `config.example.toml`). Tek bir anahtar, iki değer:
  ileride üçüncü bir kip (karar 4'ün bilet anahtarı kipi) aynı yere
  oturur; `udp_plaintext = true` gibi bir boolean oturmazdı.
- Açılırsa başlangıçta tek bir `warn` yazılır.
- Mühürlü kapıda sunucu anahtarı yoksa sessiz düz metne düşülmez;
  başlatma hatası olur (testli: `config::udp_key::tests`,
  `tests/startup_errors.rs`).

## 10. Sonraya kalanlar (karar 7, 4)

### 10.1 CID rotasyonu — tasarım (B5b'de yapılmadı)

**Neden bu turda değil:** "ucuz ve güvenli" değil. Oturum indeksi tek
CID varsayar (`table`: `cid → key`), reset jetonu CID başına
(her yeni CID'ye jeton), emeklilik ve sırasız gelen eski-CID'li kayıtlar,
göçün aday/doğrulama makinesiyle etkileşim ve iki yeni iç tür + istemci
durumu gerekir. Yanlış yapılırsa bugün kapalı olan bir şeyi açar
(başka oturumun CID'sine yönlendirme, jeton sızması). Kazancı yalnız
gizlilik (pasif bağlanabilirlik, §3 son satır); güvenlik değil.

**Tasarım (QUIC NEW_CONNECTION_ID / RETIRE_CONNECTION_ID, RFC 9000
§5.1, §9.5):**
1. **Tel:** iki taşıma çerçevesi, REL bandında (sıralı, güvenilir,
   kayıp/yeniden gönderim kuralları hazır; uygulamaya çıkmaz — istemci ve
   yazıcı onları tüketir, `UDP_ACK` gibi):
   - `NEW_CID` (s→c; yük `[u32 sıra][u64 cid][jeton 16]`, 28 B): sunucu
     yedek bir CID ve onun reset jetonunu verir.
   - `RETIRE_CID` (c→s; yük `[u32 sıra]`): istemci bir CID'yi bıraktı.
   Kayıt katmanının içinde gittikleri için şifreli ve doğrulanmış; düz
   metin kapıda yok (orada CID taşıyıcı jetondur, rotasyon anlamsız).
2. **Sunucu:** oturum başına en çok `K = 2` etkin CID (biri kullanımda,
   biri yedek). Tablo `cid → key` çoklu girdi alır; `cid_taken` hepsini
   kapsar. Yeni CID `draw_u64` ile, başka hiçbir oturumda yokken.
   Jeton = `token(yeni cid)` (aynı kapı anahtarı — stateless reset yeni
   CID için de çalışır).
3. **İstemci:** yedeği saklar; **yalnız göçte** (`rebind` ya da yeni
   yerel adres) yedeğe geçer — aynı yolda CID değiştirmek bağlanabilirliği
   azaltmaz, yalnız durum harcar. Geçişte eski CID'yi emekliye ayırır
   (`RETIRE_CID`), sunucu yeni bir yedek verir.
4. **Göç kuralı değişmez:** yeni CID'li kayıt yeni adresten gelir; açılır
   (aynı anahtar — CID kayıt anahtarını değiştirmez), en yenidir, yol
   doğrulanır. CID değişimi tek başına göç tetiklemez.
5. **Emeklilik ve sırasızlık:** emekli CID, eski yoldaki geç kayıtlar
   için bir pencere boyunca (≥ 3 × RTO ya da yol doğrulamasının 3 sn'si)
   tabloda "emekli" olarak tutulur, sonra silinir; silindikten sonra
   gelen kayıt stateless reset ALMAZ (emekli CID listesi = kısa ömürlü
   sınırlı küme; yoksa geç bir kayıt oturumu öldürürdü).
6. **Sayaçlar:** `udp_cids_issued`, `udp_cids_retired`,
   `udp_cid_retired_late` (emekli CID'li geç kayıt), istemcide
   `cids_switched`.
7. **Kalan sızıntı:** sayaç düz (aşağıda) — CID değişse de sayaç dizisi
   iki yolu bağlar. **CID rotasyonu sayaç gizlemesiz yarım kalır:**
   ikisi birlikte yapılmalı (BACKLOG satırı ikisini birlikte ister).

### 10.2 Diğerleri

- **Sayaç gizleme:** QUIC başlık koruması gibi, sayacı (ve faz bitini)
  şifreli örnekten türetilen maskeyle örter. Bugün sayaç düz görünür:
  gözlemciye paket hızını, kaybı ve (rotasyon gelince) yollar arası
  bağı söyler; reset datagramının rastgele sayacı da onunla gizlenir.
  Alıcı sayacı maskeyi çözmeden bilemediği için replay penceresi ve faz
  kuralı sırası değişir: incelemeyle birlikte tasarlanmalı.
- **Bilet anahtarı kipi:** netcode tarzı, FS yok. Şimdi yok; istenirse aynı
  kayıt katmanına ikinci el sıkışma kipi olarak eklenir.
- **Adres doğrulama jetonu:** QUIC Retry/NEW_TOKEN gibi, yeniden bağlanmada
  çerez turunu atlamak için. Stateless reset sonrası yeniden bağlanma
  bugün tam çerez turu (+1 RTT) yapar.
- ~~**İstemci kütüphanesinde EOF**~~ — B128 ile kapandı (2026-10-03):
  `gsb_client::Conn::recv` rUDP'de de oturumun sonunu `Recv::Closed`
  olarak döndürür (önce alınmış kareler boşaltılır), nedeni
  `UdpClient::ended()` (`RelDead`, `Reset`, `SealLimit`); yük üreteci her
  bitişi nedeniyle bir kez sayar (`udp_ends_*`). DESIGN §6 "İstemci
  tarafında oturumun sonu".

## 11. Dış güvenlik incelemesi (karar 8, D13) — devir kapsamı

> **Kullanıcı kararı (2026-10-03):** dış inceleme yapılmayacak — proje
> kendi kullanımımız (ve GitHub'da açık bir referans) için. Bu bölüm iç
> gözden geçirme kontrol listesi olarak ve ileride bir inceleme istenirse
> devir kapsamı olarak kalır.

Bu bölüm incelemeciye olduğu gibi verilecek kapsam dokümanıdır.
**Ne zaman:** şimdi — kayıt katmanı rUDP'ye bağlı (B5a), politika ve
reset bağlı (B5b); hat kapandı. İnceleme referansı: bu dalın birleştiği
`main` commit'i.

### 11.1 Sistem bir paragrafta

Oyun sunucusu motorunun UDP protokolü (rUDP). Durumsuz çerez el
sıkışmasının (HELLO → challenge → proof → accept) proof/accept adımına
Noise `NK_25519_ChaChaPoly_BLAKE2s` biner (0 ek RTT; istemci sunucunun
statik açık anahtarını sabitler). Sonra oturumun her datagramı iki yönde
kendi kayıt katmanımızla mühürlenir: `[kind][cid c→s][sayaç u64]
[şifreli][tag 16]`, ChaCha20-Poly1305, başlık = AAD, nonce = sayaç,
1024'lük replay penceresi, Noise REKEY'li anahtar fazları (bir önceki
anahtar toleransı), RFC 9001 §6.6 bütünlük sınırı. CID'le yönlendirme
ve RFC 9146 §6 biçimli göç (doğrulanmış + en yeni + yolu doğrulanmış).
Stateless reset HMAC jetonuyla. Tek kullanıcısı kendi istemcilerimiz
(Rust; ileride C#/Unity portu). Standart uyumu yok (karar 2).

### 11.2 Kapsam (dosyalar)

| # | Alan | Dosyalar (`crates/gsb-net/src/…`) | Boy |
|---|---|---|---|
| 1 | Kripto çekirdeği (sans-IO) | `seal/` (test hariç): `handshake.rs`, `identity.rs`, `key.rs`, `sealer.rs`, `opener.rs`, `replay.rs`, `reset.rs`, `reset/datagram.rs`, `wire.rs` | ~1210 satır, yorumsuz ~760 |
| 2 | El sıkışmanın çereze binişi | `udp/demux/handshake.rs`, `udp/demux/noise.rs`, `udp/sealed.rs`, `udp/sealed/budget.rs`, `udp/cookie.rs`, `udp/client/handshake.rs` | |
| 3 | Kayıt yolu ve göç | `udp/demux/record.rs`, `udp/demux/migrate.rs`, `udp/path.rs`, `udp/demux/table.rs`, `udp/writer/seal.rs`, `udp/client/seal.rs`, `udp/client/io.rs` | |
| 4 | Anahtar fazı politikası ve ACK → sayaç | `udp/sealed/rekey.rs`, `udp/writer/{send, reliable}.rs`, `udp/client.rs` (`send_frame`) | |
| 5 | Stateless reset | `udp/sealed/door.rs`, `udp/demux/reset.rs`, `seal/reset{,/datagram}.rs`, `udp/client/seal.rs` (`stateless_reset`) | |
| 6 | Anahtar yönetimi | `gsb-server/src/config/udp_key.rs` (statik ve reset anahtarı, hata metinleri), `gsb-server/src/boot/{start, accept}.rs` | |

2–6'nın kripto dokunan kısmı ~2300 satır (testler hariç, yorumlu).

### 11.3 İncelemecinin doğrulaması istenen değişmezler

1. **Nonce asla tekrar etmez:** yön başına bir anahtar zinciri, sayaç
   monoton, fazlar boyunca sürer, 2^62'de sert ret (sarma yok); REKEY
   nonce'u (2^64−1) kayıt sayacının erişemeyeceği yerde. Her yeniden
   gönderim yeni sayaç (§15 karar 2).
2. **Başlık bütünlüğü:** başlık AAD; `Sealer::seal` başlığı kendisi yazar
   (başlık ile nonce ayrışamaz).
3. **Ret sırası ve sayımı** (§5): ucuz kontroller AEAD'den önce;
   pencere yalnız doğrulanmış kayıtla ilerler; sahte datagram yuva
   yakamaz; bütünlük sınırı tüm anahtarlar üzerinden.
4. **Anahtar fazı:** gönderen kuralları (mesafe ≥ 1024, onay) + açıcının
   tek önceki anahtarı ⇒ dürüst eşin kaydı asla anahtarı tutulmayan bir
   nesilden gelmez (§6; seeded model testi). ACK → sayaç eşlemesinin
   yalnız ilk gönderime kefil olması; onaysız eşin oturumu durdurmaması.
5. **DH yalnız çerezden, msg1 biçiminden, kaynak sınırından ve DH
   bütçesinden sonra** (§4); prologue'un çerez alışverişine bağlanması;
   idempotent proof (saklanan msg2) ve sahte accept toleransı.
6. **Göç:** yalnız açılan + en yeni + yolu doğrulanmış kayıt adresi
   taşır; doğrulanana kadar s→c eski yolda; 3× bütçe; koklanmış CID ile
   sahte kayıt hiçbir şey başlatmaz (§7).
7. **Stateless reset:** jetonun yalnız istemci ve sunucuda olması;
   kapıya bağlamanın, bir kapının başka kapının canlı oturumunun
   jetonunu vermesini engellemesi; resetin tetikleyenden kesin kısa
   olması ve döngünün sonlanması; oranın her işten önce olması; istemci
   karşılaştırmasının sabit zamanlı olması ve yalnız açılamayan
   reset boyundaki SEALED datagramda yapılması (§8).
8. **Anahtar hijyeni:** `Zeroizing` / `zeroize` kullanımı; hiçbir
   `Debug`'ın, log'un ya da hata metninin anahtar baytı taşımaması
   (statik, reset, oturum anahtarları, jeton); reset anahtarının statik
   anahtardan türetilmesinin (HMAC, etiketli) statik anahtarın DH
   kullanımıyla etkileşmemesi.
9. **Sayaçların tam olması** ("her kaybı say"): her ret, her ertelenen
   rekey, her reset kendi adıyla.

### 11.4 Bilinçli kararlar ve kabul edilen riskler (incelemeci sorgulasın)

- DTLS yerine kendi kayıt katmanı (§1) — standart uyum yok.
- Sayaç ve faz biti düz (§10.2); CID ağlar arası sabit (§10.1).
- Demux ACK'ini ve challenge'ı yazıcı üzerinden mühürlemek (tek sayaç
  alanı, §15 karar 1).
- Reset anahtarının varsayılan olarak statik anahtardan türetilmesi
  (§8); sızan reset anahtarı = hizmet reddi (oturum bitirme), gizlilik
  değil.
- Reset boyundaki her yanlış jetonun kendi `seal_*` adıyla ayrıca
  `stateless_resets_invalid` sayılması.
- Rekey'in simetrik zincir olması (yeni DH yok): oturum içi ileri
  gizlilik yalnız geçmiş fazlar için.
- `snow`'un denetlenmemiş olması (yalnız el sıkışma; §11.6).

### 11.5 Giriş noktaları ve kanıt

- Testler: `cargo test -p gsb-net --lib seal` (çekirdek: RFC 8439 ve
  cacophony vektörleri, REKEY = snow, her ret, seeded model),
  `cargo test -p gsb-net --lib udp` (bağlama, göç, politika, reset,
  gerçek soket), `cargo test -p gsb-server --test rudp_resume` (her akış
  mühürlü/düz metin kapıda; yeniden başlatma). Zamanlama sondaları:
  `seal::tests::cost`, `udp::demux::tests::sealed::cost` (`--ignored`).
- Mutasyon kanıtları her turun CHANGELOG girdisinde (x1, B3, B5a, B5b).
- Ölçümler: §4 (B110), §15 (B5a).

### 11.6 Kapsam dışı

- `snow` (yalnız el sıkışma; denetlenmemiş, tek bakımcı — gerekirse
  `clatter`'a ya da ~200 satırlık kendi NK'mıza geçilir), RustCrypto
  AEAD (NCC Group 2019–20 incelemesi, bulgu yok), `curve25519-dalek`,
  `blake2`, `hmac`, `subtle`, `zeroize`.
- Uç noktaların ele geçirilmesi, sunucu statik anahtarının sızması
  (§3 "Kapsam dışı"), trafik analizi.
- Düz metin kapı (`udp_security = "plaintext"`, dev/LAN): hiçbir şeyi
  korumaz, tasarım gereği.
- C# portu (gelince ayrı inceleme: aynı vektörler — cacophony NK + bizim
  SEALED, REKEY, reset vektörlerimiz).

### 11.7 İncelemeden beklenen

Bulgu listesi (önem derecesiyle), §11.3'ün her maddesi için "doğrulandı
/ bulgu" hükmü, §11.4'teki kararlardan değiştirilmesi önerilenler, ve
C# portu için ek vektör önerileri. Bulgular BACKLOG'a satır olarak girer.

## 12. Tur sırası ve durum: x1 → B3 → B89 → B5a → B5b → D13

| Tur | Ne yapar |
|---|---|
| **x1** | **Yapıldı (2026-10-02):** `gsb_net::seal` çekirdeği (el sıkışma sarmalayıcısı, `Sealer`/`Opener`, replay penceresi, anahtar fazı, reset jetonu, SEALED başlık kodlaması) + bu doküman. **Bağlanmadı:** rUDP'nin hiçbir yolu bu modülü çağırmaz |
| **B3** | **Yapıldı (2026-10-02):** kriptosuz CID ve göç (opt-in `udp_migration`): proof'a caps baytı, accept'te CID, etiketli c→s datagramı, PATH_CHALLENGE/RESPONSE, 3x bütçe, oturumlara iç anahtar + `addr→key` / `cid→key` indeksleri, writer'a `UDP_PATH` (`PathChanged`), `UdpClient::rebind()`, 15 sayaç; kind haritası kesin (§5). Ayrıntı §7, DESIGN §6 "Bağlantı göçü" |
| **B89** | **Yapıldı (u89, 2026-10-02):** kaynak başına bekleyen oturum sınırı (`max_handshakes_per_source`, D11'in anahtarı; çerezden sonra, DH'den önce; `udp_proofs_refused_per_source`), göçte kaynağın taşınması (B113: aktörün `peer`'i, registry'nin D12 sayımı, demux'ın bekleyen yeri; dolu kaynağa taşınmaz — `unauth_source_moves_kept`, `udp_pending_source_moves_kept`), B110 ölçümü (§4). **B5a'ya devredildi:** DH'den önce küresel el sıkışma bütçesi (§4 tasarımı) — kaynak başına sınır hızı kesmez. SECURITY §4.3.3 |
| **B5a** | **Yapıldı (2026-10-02):** DH'den önce küresel el sıkışma bütçesi (§4; B89'dan); msg1 proof'a, msg2 accept'e; SEALED kayıt; sunucu statik anahtarı config'den; `Sealer` writer'a, `Opener` demux'a (demux'ın kendi gönderdikleri `UDP_SEND` ile yazıcıdan); `Refusal` adları sayaçlara; göç kuralının üç koşulu; PATH_* şifreli iç kind; mühürlü kip varsayılan, düz metin dev/LAN anahtarı; demux'ta çözme CPU'sunun ölçümü. Ayrıntı §15 |
| **B5b** | **Yapıldı (2026-10-03):** anahtar fazı politikası (2 dk / 2^20 kayıt; REL ACK'iyle ilk-gönderim onayı; onaysız eş sayılır, durdurmaz — §6), stateless reset (config'de ya da statik anahtardan türetilen, kapıya bağlı anahtar; tetikleyenden kısa, oranlı reset; istemcide sabit zamanlı kontrol, tek ad — §8), 5 sayaç. CID rotasyonu tasarlandı, yapılmadı (§10.1). Ayrıntı §16 |
| **B7** | **Kapandı (B5a ile):** `rudp_resume.rs`'in her rUDP akışı mühürlü ve düz metin kapıda koşar (RECONNECT §5). B5b yeniden başlatma akışını ekledi: reset, < 1 sn'de bitiş, aynı adla yeniden katılma |
| **D13** | **Sırada:** dış inceleme, kapsam §11 |

## 13. Kripto çekirdeği (`crates/gsb-net/src/seal/`; x1, B5a ve B5b eklemeleri)

**Modüller:**

| Dosya | İçerik |
|---|---|
| `mod.rs` | Modül belgesi, dışa açılanlar |
| `identity.rs` | `NOISE_PATTERN`, boy sabitleri, `StaticKey`, `Accept`, `HandshakeError` |
| `handshake.rs` | `Initiator` (istemci), `Msg1` → `Responded` (sunucu), `Session::into_halves` |
| `key.rs` | Faz anahtarı: Noise nonce kodlaması, mühürle/aç, REKEY |
| `sealer.rs` | `Sealer`, `SEAL_LIMIT`, `REKEY_MIN_DISTANCE`, `SealError` |
| `opener.rs` | `Opener`, `Refusal`, `Opened`, `INTEGRITY_LIMIT` |
| `replay.rs` | `REPLAY_WINDOW` bit halkası |
| `reset.rs` | `ResetKey` (`from_bytes`, `derived_from`, `for_door`, `token`), `ResetToken` (sabit zamanlı karşılaştırma) |
| `reset/datagram.rs` | Reset datagramının düzeni: `reset_datagram`, `reset_tail`, `RESET_LEN_{MIN,MAX}` (B5b) |
| `wire.rs` | SEALED başlık sabitleri ve kodlama/çözme |

**B5a'nın çekirdeğe eklediği** (vektörler ve testler değişmedi, yeşil):
- `StaticKey::private_bytes()` — özel yarı, `Zeroizing` içinde (anahtar
  dosyası/efemeral test ve yük koşusu config'i için; asla loglanmaz).
- `impl Debug for StaticKey` — yalnız açık yarıyı yazar (`UdpSecurity`'nin
  `Debug`'ı hiçbir anahtar yazmaz); testli.
- Test kancaları `Sealer::set_next_counter_for_test`,
  `Opener::set_forged_for_test` `pub(super)` → `pub(crate)`
  (`#[cfg(test)]`; rUDP'nin sınır testleri onları kullanır).
- Modül belgeleri "bağlı değil" yerine "B5a'da bağlandı".

**B5b'nin çekirdeğe eklediği** (mevcut vektörler ve testler değişmedi,
yeşil; `ResetKey::token` bayt bayt aynı):
- `ResetKey::derived_from(&StaticKey)` — `HMAC-BLAKE2s(statik özel
  anahtar, "gsb-rudp-reset-key/1")`; `ResetKey::for_door(&[u8])` —
  `HMAC-BLAKE2s(anahtar, "gsb-rudp-reset-door/1" ‖ kapı)` (§8).
- `reset_datagram`, `reset_tail`, `RESET_LEN_MIN` = 26, `RESET_LEN_MAX` =
  41 (§8).
- `impl Debug for ResetKey` — anahtarı yazmaz (testli).
- `Sealer`/`Opener` değişmedi: politika ve ACK eşlemesi çekirdeğin
  dışında (`udp::sealed::rekey`), çekirdeğin iki rekey kuralını kapı
  olarak kullanır.

**Bağımlılıklar** (hepsi saf Rust; `ring` yalnız mevcut quinn/rustls
yolundan gelir, yenilerden hiçbiri getirmez):

| Crate | Sürüm | Lisans | Not |
|---|---|---|---|
| `snow` | 0.10.0 | Apache-2.0 OR MIT | `default-features = false`; yalnız `use-curve25519`, `use-chacha20poly1305`, `use-blake2`, `use-getrandom`, `risky-raw-split`. Varsayılan `std` özelliği `"ring/std"` der ve `ring`'i çeker. `default-resolver-crypto` da `ring`'siz, ama AES-GCM ve SHA-2'yi boşuna getirirdi |
| `chacha20poly1305` | 0.10.1 | Apache-2.0 OR MIT | snow'un kullandığı hat: tek kopya derlenir |
| `blake2` | 0.10.6 | MIT OR Apache-2.0 | snow zaten çekiyor |
| `hmac` | 0.12.1 | MIT OR Apache-2.0 | `SimpleHmac<Blake2s256>` |
| `subtle` | 2.6.1 | BSD-3-Clause | Kilitte zaten var |
| `zeroize` | 1.9.0 | Apache-2.0 OR MIT | Kilitte zaten var |
| *(geçişli)* `curve25519-dalek` 4.1.3, `getrandom` 0.3.4, `chacha20` 0.9.1, `poly1305` 0.8.0, `fiat-crypto` 0.2.9 | | BSD-3 / MIT / Apache | C yok, `cc` yok |

**Testler** (`cargo test -p gsb-net --lib seal`):
- Bilinen-cevap testleri:
  - RFC 8439 §2.8.2;
  - cacophony `Noise_NK_25519_ChaChaPoly_BLAKE2s` vektörü bizim
    sarmalayıcımızdan: msg1, msg2, handshake hash, dört taşıma mesajı,
    statik açık anahtar türetimi;
  - REKEY = snow'un `rekey_outgoing`'i.
- El sıkışma: gidiş-dönüş, yanlış sabitlenmiş anahtar, bağlam uyuşmazlığı,
  boy retleri DH'den önce, sahte/yabancı accept'ten sonra tamamlanma.
- Kayıt:
  - her ret adı;
  - her bayt × bit çevirmesi;
  - pencere kenarları, bit halkasının sarması, uzak sıçrama;
  - sayaç sınırı, bütünlük sınırı;
  - faz sınırında sırasızlık, önceki anahtarın tam sınırda bırakılması.
- Seeded model testi (SplitMix64): sırasızlık, bekletme, düşürme, çoğaltma,
  sahteleme. Her hüküm anahtar bilmeyen bir modelle karşılaştırılır.
- Reset (B5b, `tests/reset.rs`): türetmelerin yeniden başlatmada aynı,
  başka anahtar/kapıda farklı olması; 0..=1472 her tetikleyen boyu için
  kesin kısa reset ya da hiç; düzen (kind, sayaç < 2^62, dolgu, jeton);
  açıcının reseti `Forged` reddetmesi ve kuyruğun jetonu taşıması.

## 14. Kaynaklar

- **Noise:** https://noiseprotocol.org/noise.html (§4.2 ve §11.3 REKEY, §12.3
  ChaChaPoly nonce'u, §7.7 yük güvenliği), https://github.com/mcginty/snow
- **WireGuard:** https://www.wireguard.com/protocol/
- **RFC'ler:**
  - RFC 8439 (ChaCha20-Poly1305);
  - RFC 9000 §10.3 (stateless reset), §12.3, §8.2 (yol doğrulaması);
  - RFC 9001 §6 (anahtar güncelleme), §6.6 (kullanım sınırları);
  - RFC 9146 §6 (CID ile adres güncelleme);
  - RFC 9147; RFC 9853; RFC 6479 (replay penceresi).
- **DTLS adayları:**
  - https://github.com/algesten/dimpl
  - https://github.com/webrtc-rs/rtc
  - https://github.com/rustls/rustls/issues/40
- **RustCrypto denetimi:**
  https://www.nccgroup.com/research-blog/public-report-rustcrypto-aesgcm-and-chacha20pluspoly1305-implementation-review/
- **netcode:** https://github.com/mas-bandwidth/netcode/blob/main/STANDARD.md
- **C#:** https://github.com/bcgit/bc-csharp

## 15. B5a: bağlama (2026-10-02)

**Kod:** `crates/gsb-net/src/udp/sealed.rs` (kip `UdpSecurity`, el
sıkışma teli, `UDP_SEND` yükü, sayaçlar) + `sealed/budget.rs` (DH
kovası); `udp/demux/noise.rs` (proof → msg1 biçimi → kova → CID → DH),
`udp/demux/record.rs` (SEALED yönlendirme, açma, retler, göç kuralı,
bütünlük sınırında kapanış), `udp/writer/seal.rs` (her s→c datagramı
mühürlenir; `UDP_SEND`; sayaç tavanında kapanış), `udp/client/seal.rs` +
`client/handshake.rs` (istemci NK, sabitlenmiş anahtar, retler);
`gsb-server` `config/udp_key.rs` (anahtar, kip). Tel tabloları ve
uyumluluk matrisi DESIGN §6 "Kayıt katmanı"nda.

**Kararlar ve gerekçeleri:**
1. **Demux'ın gönderdikleri yazıcıdan (`UDP_SEND`, opcode 16).** Demux
   ACK'i ve PATH_CHALLENGE'ı doğrudan gönderiyordu; mühürlü kapıda s→c
   `Sealer` yazıcıdadır. Demux'a ikinci bir `Sealer` vermek (ayrı sayaç
   alanı ya da alt anahtar) reddedildi: istemcinin TEK replay penceresi
   farklı hızla ilerleyen iki sayaç alanını kaldıramaz (yavaş taraf
   `TooOld` olurdu), alt anahtar ise yönü ikiye böler ve Opener'ı
   değiştirirdi. Bedel: ACK başına bir kanal atlaması (µs) ve dolu kanalda
   kayıp (sayılı: `udp_acks_not_queued`); istemci yeniden gönderir.
2. **Güvenilir band iç datagramı tutar, her gönderim yeniden
   mühürlenir.** Aynı sayaçla yeniden göndermek alıcının replay
   penceresinde düşer; mühürlü baytı saklamak nonce'u tekrar etmezdi ama
   işe yaramazdı. Yazıcı ve istemci aynı kuralı izler (`wire`).
3. **İdempotent proof: accept saklanır, ilk açılan kayıtta bırakılır.**
   msg1 karşılaştırılmaz: aynı adres + nonce'a bağlı doğrulanan çerez
   aynı el sıkışmadır; sahte msg1'e saklanan msg2 saldırgana bir şey
   vermez. Oturum boyu saklamak 100k'da ~7,7 MB olurdu, gereksiz.
4. **Mühürlü kapıda CID her oturuma.** Kayıt başlığı CID'yi taşır;
   CID'siz mühürlü oturum yönlendirilemez. Entropi başarısızsa oturum
   kurulmaz (sayılı `udp_entropy_draws_failed`), zayıf değer yok.
   `udp_migration = false` mühürlü kapıda CID'yi kaldırmaz, yalnız başka
   adresten geleni okumaz (`udp_datagrams_no_session`).
5. **Reset jetonu alanı dolu, kapı başına rastgele anahtardan.** Tel
   (msg2'nin 24 B'lik yükü) B5b'de değişmesin diye; anahtarın config'e
   taşınması (karar 9) ve jetonun kullanımı B5b (yapıldı, §8, §16).
6. **Anahtar biçimi: 64 hex, satır içi ya da dosya.** `udp_cookie_key`
   ile aynı yazım (config'de ham anahtar baytları için tek biçim);
   base64 (WireGuard) ek kod ve ikinci biçim olurdu. Dosya yolu üretim
   için (anahtar config dosyasının dışında). Hata metinleri anahtarın
   hiçbir karakterini taşımaz (testli). Testler ve yük üreteci çalışma
   anında üretir (`gsb_server::ephemeral_udp_key`) — sertifikalar gibi
   depoya anahtar girmez.
7. **Bütçe varsayılanı 1000/s, kova 50 ms.** Ölçülen ~130–180 µs/el
   sıkışma (yüklü makine) ile demux'ın ~%13–18'i; 1000 oyunculuk
   fırtına ~1 sn'de girer (ölçüm aşağıda: p99 1,0–1,4 sn; bütçesiz
   ~0,3 sn). Kova küçük tutuldu: büyük kova fırtınada demux'ı yüz
   milisaniyelerce DH'ye bağlardı.
8. **Mühürlü istemci düz metin kabulü görünce hemen bırakmaz.** Kabul
   imzasızdır, yol dışı saldırgan sahteleyebilir; hemen bırakmak el
   sıkışmayı öldürmeye açık olurdu. Sayar, bekler; süre dolunca yalnız
   düz metin kabul gördüyse `ConnectionRefused` ("düz metin kapı?").
9. **Bütünlük sınırı ve sayaç tavanı oturumu kapatır** — yeni kapanış
   nedeni açılmadı: `stream_rejected` ("taşıma akışı reddetti") anlamca
   tutar; ayrıntı `udp_sessions_ended_seal_limit` ve gerekçe metninde.

**Ölçüm (B5a, 2026-10-02, release; Ryzen 9 7950X, 32 iş parçacığı,
`rmem_max` 4 MiB).** **Yük notu, dürüstçe:** makinede başka işler
koşuyordu; 1 dk yük ortalaması 20 dk beklemede de 5'in altına inmedi —
koşular yük **11–38** iken alındı (her satırda başlangıç yükü). Sayılar
sessiz makinede daha iyi olur; oranlar ve sıralama anlamlıdır.

*El sıkışma CPU'su (B110 sondası `seal::tests::cost`, değişmedi; yük
~33, 3 koşu):* responder el sıkışması **130–136 µs** (çekirdek-saniyede
~7,3–7,7 bin), istemcinin msg1'i 64–67 µs, bir X25519 39–40 µs, oran 3,4.
u89'un 175–186 µs'si (yük ~36) ile aynı sınıf.

*Demux'ın datagram başına CPU'su (yeni sonda
`udp::demux::tests::sealed::cost`, 40 B'lik girdi, 200k datagram, yük
~22–23, 3 koşu):* düz metin **132–136 ns**, mühürlü **1300–1355 ns**
(76 B kayıt): **kayıt katmanı datagram başına ~1,17 µs ekler** (açma =
ChaCha20-Poly1305 + düz metnin `Vec`'i + kopya). Soketten okuma
(`recv_from`, ~0,1–0,3 µs) iki kolda da ayrıca.

*100k çıkarımı (B107'nin sorusu):* demux tek görev. Oturum başına
saniyede `r` gelen datagramla ek açma CPU'su `100k × r × 1,17 µs`:
`r = 2` → **0,23 çekirdek**; `r = 10` (150 ms'lik girdi + ACK + rapor —
loadgen'in profili) → **1,17 çekirdek**, toplam demux işi
(`~1,3 µs + recv`) ~1,5–1,6 çekirdek: **tek demux 100k mühürlü oturumu
10 dg/s'de taşıyamaz**; bu ölçümle tavan ~600–700 bin datagram/s/çekirdek
≈ 10 dg/s'de ~60–70k oturum. Çözüm yolları (b5a satırı, BACKLOG):
tahsissiz yerinde açma (`Opener::open_in_place` — tahsis ve kopya
gider), açmayı demux'tan almak (oturum başına işçi ya da kapının
paylaştırılması; B121'in DH için önerdiği işçi biçimiyle birlikte).

*rUDP katılma fırtınası* (`gsb-loadgen 1000 --orchestrate --procs 2
--visibility spatial --duration 8 --transport udp --udp-security
{plaintext|sealed} [--udp-recv-buffer 4194304]
[--udp-handshakes-per-sec N]`; her koşuda `connected = joined = 1000`,
`errors = 0`, `seal_forged = 0`):

| Kol | arabellek | yük | connect p50 / p99 ms | `hs_retries` | `udp_proofs_refused_budget` | `RcvbufErrors` |
|---|---|---|---|---|---|---|
| düz metin | varsayılan | 11 · 23 · 22 | 71/556 · 61/574 · 65/363 | 1360 · 1394 · 1322 | — | 4909 · 4931 · 5056 |
| düz metin | 4 MiB | 11 · 28 · 38 | 50/81 · 47/105 · 54/125 | 128 · 12 · 96 | — | 0 · 0 · 0 |
| mühürlü (bütçe 1000/s) | varsayılan | 20 · 23 · 29 | 777/2190 · 968/2384 · 965/2382 | 5550 · 5869 · 5847 | 2793 · 3166 · 3439 | 4977 · 5524 · 5064 |
| mühürlü (bütçe 1000/s) | 4 MiB | 23 · 25 · 37 | 476/1189 · 464/1379 · 483/1045 | 3136 · 3080 · 3095 | 3084 · 3024 · 3095 | 0 · 0 · 0 |
| mühürlü, bütçe YOK (`0`) | 4 MiB | 33 · 33 · 36 | 178/335 · 147/298 · 165/299 | 1092 · 876 · 1193 | 0 | 0 · 0 · 0 |
| mühürlü, bütçe 5000/s | 4 MiB | 32 · 35 · 36 | 151/280 · 190/318 · 165/305 | 677 · 1264 · 756 | 0 | 0 · 0 · 0 |

Sunucu CPU'su (`server_cpu_s`, 8 sn'lik koşunun tamamı): düz metin
5,9–12,3, mühürlü 10,1–15,6 (yükte gürültülü; fark el sıkışma DH'si +
açma + mühürleme).

**Okuma.** (1) Mühürlü fırtınanın gecikmesini **bütçe** belirliyor, DH
değil: 1000 el sıkışma 1000/s'de en az ~1 sn — p99 4 MiB'de 1,0–1,4 sn;
bütçesiz ya da 5000/s'de p99 ~0,3 sn (düz metnin 4 MiB'lik 0,08–0,13
sn'sine karşı: +~0,2 sn = 1000 × ~135 µs'lik DH'nin demux'ta seri
işlenmesi ve istemcinin kendi DH'leri). (2) Reddedilen proof istemci
başına ~3: kova 50 ms'lik, yeniden gönderimler ≤ 200 ms'de. (3)
Varsayılan arabellekte çekirdek düşüşleri iki kipte aynı sınıf (~5000);
mühürlü proof/accept daha büyük olsa da düşüşü bütçenin yeniden
gönderimleri büyütür. **Karar (bu tur):** varsayılan 1000/s kaldı —
sözleşmenin değeri ve gerekçesi (demux'ın ≤ ~%14–18'i DH'ye); fırtına
p99'u ~1 sn uzar. Hızlı katılma isteyen dağıtım bütçeyi yükseltir
(`udp_handshakes_per_sec`, OPS §2); seçim kullanıcıya (BACKLOG b5a
satırı).

**B5b'nin devraldığı** (ilk üçü B5b'de yapıldı ya da tasarlandı — §16; B120/B121 ölçüme bağlı açık):
- Rekey politikası (ne zaman) ve ACK → sayaç eşlemesi
  (`Sealer::note_peer_ack`): REL ACK'i hangi kayıt sayacına karşılık
  geliyor — yazıcı her REL gönderiminin sayacını tutmalı (yeniden
  gönderim yeni sayaç alır).
- Stateless reset: config'de 32 B reset anahtarı (karar 9; bugün kapı
  başına rastgele), bilinmeyen CID'li SEALED datagramına kısa ve oranlı
  reset (demux'ta bugün `udp_cid_unknown` sayılıp düşer — kanca orası),
  istemcide `Forged` sonrası sabit zamanlı jeton kontrolü ve sayaç adı.
- CID rotasyonu / adres doğrulama jetonu (opsiyonel).
- Kova adaleti (B120) ve DH'yi işçi havuzuna taşıma (B121) — ölçüme
  bağlı.
- Dış inceleme (D13) bu turun kodunu da kapsar (§11.2).

## 16. B5b: anahtar fazları ve stateless reset (2026-10-03)

**Kod:** `crates/gsb-net/src/udp/sealed/rekey.rs` (`RekeyPolicy`,
`SendHalf`: politika + ACK → sayaç eşlemesi), `udp/sealed/door.rs`
(`DoorSeal`: reset anahtarı, reset kovası, politika; `configure`),
`udp/demux/reset.rs` (bilinmeyen CID → reset), `udp/client/seal.rs`
(`stateless_reset`, istemcinin `SendHalf`'ı), `udp/writer/{seal, send,
reliable}.rs` (ilk gönderimin sayacı, ACK), `udp/transport/config.rs`
(`reset_key`, `stateless_resets_per_sec`, `rekey`), `seal/reset.rs` +
`seal/reset/datagram.rs`; `gsb-server` `config/udp_key.rs`
(`udp_reset_key[_file]`), `config/axes/listeners.rs`
(`udp_stateless_resets_per_sec`).

**Kararlar ve gerekçeleri:**
1. **Tetik 2 dk ya da 2^20 kayıt, iki yön bağımsız** (§6 "Politika").
   Rekey bir sınırın cevabı değil; ele geçen anahtarın açtığını daraltır.
   Config anahtarı yok: güvenlik politikası motorundur; kütüphane
   kullanıcısı `RekeyPolicy` verebilir (testler kısa politika kullanır).
2. **Onay = REL ACK'inin kapsadığı ilk gönderim sayacı** (§6). Yoklama
   (PROBE/REPORT) da bir onay kaynağı olabilirdi (yeniden gönderilmez,
   rapor adını taşır); gerekmedi — heartbeat REL'dir ve canlı oturumda iki
   yönü de besler. REL trafiği hiç olmayan oturum rekey etmez, sayılır.
3. **Sayım kısılır** (10 sn'de bir): her mühürlemede sayılsa canlı ama
   REL'siz bir oturum sayacı saniyede yüzlerce artırırdı (anlam aynı,
   gürültü değil bilgi istenir). Mesafe ertelemesi hiç sayılmaz (normal).
4. **Reset anahtarı opsiyonel, yoksa statik anahtardan türetilir, kapıya
   bağlanır** (§8). Taslağın "yoksa başlatma hatası"ndan sapma: zorunlu
   yeni anahtar her dağıtımı kırardı, türetme aynı kalıcılığı ek sır
   olmadan verir.
5. **Reset düzeni SEALED s→c kaydı biçiminde, ≤ 41 B, tetikleyenden
   kısa** (§8). Sayaç düz gittiği için tam ayırt edilemezlik yok; ucuz
   olan (kind, boy, rastgele gövde) yapıldı, gerisi sayaç gizlemeyle.
6. **İstemcide tek ad** (`stateless_resets_received`; §8'in açık
   sorusunun cevabı) + "bunlardan" `stateless_resets_invalid`.
7. **Oran 10 000/s, `0` = kapalı** (§8): yeniden başlatma fırtınasını
   ~1 sn'de bitirir, demux'ın ~%3'ü. Tahmin, ölçülmedi (BACKLOG b5b
   satırı).
8. **CID rotasyonu yapılmadı** (§10.1): ucuz ve güvenli değil; sayaç
   gizlemesiz yarım kalır.

**Tel:** değişen yok, eklenen tek datagram reset (§8 tablosu). El
sıkışma, kayıt ve msg2 B5a'dakinin bayt bayt aynısı (jeton alanı B5a'da
ayrılmıştı). Eski (B5a) istemci reseti tanımaz: açamadığı kayıt olarak
`seal_forged` sayar, 5 sn REL sınırıyla biter — uyumlu.

**Sayaçlar** (taşıma tablosunun sonuna 5; OPS §3): `udp_rekeys`,
`udp_rekeys_unconfirmed` (yazıcı), `udp_stateless_resets_sent`,
`udp_stateless_resets_rate_limited`, `udp_stateless_resets_send_failed`
(demux). İstemci `UdpClientStats`: `rekeys`, `rekeys_unconfirmed`,
`stateless_resets_received`, `stateless_resets_invalid`.

**Config:** `udp_reset_key` / `udp_reset_key_file` (64 hex, opsiyonel;
ikisi birden ya da bozuk → başlatma hatası `rUDP reset key: …`, anahtar
yankılanmaz), `udp_stateless_resets_per_sec` (vars. 10 000, `0` = yok).
Düz metin kapı ikisini de okumaz.

**Ölçüm:** bu turda yeni ölçüm yok. Rekey bir ChaCha20 bloğu (iki
uçta), oturum başına ~2 dk'da bir — ihmal edilir; reset maliyeti
tahmin (yukarıda).

