; Breadbin-REU-Lader: lädt eine REU-Datei vom Stick des Ultimate ins REU (ab Adresse 0) und meldet
; das Ergebnis in $02A7. Breadbin startet ihn per REST (runners:run_prg), liest die Marke per
; readmem und startet danach das eigentliche Spiel (der Inhalt des REU übersteht den Reset).
;
; Bauen: 64tass --cbm-prg -a -o reu.prg reu.asm
;
; Breadbin trägt Länge und vollen Pfad (ASCII, z. B. /USB0/Breadbin/spiel.reu) in den Parameterblock
; am Ende ein (Kennung „BBREUPAR“). Ladeweg nur über das DOS-Ziel der UCI (OPEN_FILE, LOAD_REU,
; CLOSE_FILE) – CTRL_CMD_LOAD_REU ($04 $08) wertet den Namen nicht aus.
;
; Marke $02A7: $01 läuft, $A5 fertig, $E0 keine UCI (Command Interface aus), $E1 Datei nicht
; geöffnet, $E2 Laden gescheitert. Statustexte der drei Befehle ab $0340 (je 16 Byte).

MARK    = $02a7
RES     = $0340
UCI     = $df1c
READY   = $a474
zp      = $fb
cnt     = $f7           ; 3 Byte
tmp     = $fa

        * = $0801
        .word ende_basic, 10
        .byte $9e
        .text "2061"
        .byte 0
ende_basic
        .word 0

start   sei
        lda #1
        sta MARK
        lda UCI+1               ; Kennung der UCI
        cmp #$c9
        beq +
        lda #$e0
        jmp ende
+       jsr uci_reset
        ; OPEN_FILE $01 $02 $01 <Pfad>
        lda plen
        clc
        adc #3
        tax
        ldy #<c_open
        lda #>c_open
        jsr uci_cmd
        ldx #$00
        jsr status_merken
        lda RES
        cmp #'0'
        beq +
        lda #$e1
        jmp ende
+       ; LOAD_REU $01 $21 <Adresse 4> <Länge 4>
        ldx #10
        ldy #<c_load
        lda #>c_load
        jsr uci_cmd
        ldx #$10
        jsr status_merken
        ; CLOSE_FILE $01 $03
        ldx #2
        ldy #<c_close
        lda #>c_close
        jsr uci_cmd
        ldx #$20
        jsr status_merken
        lda RES+$10             ; „00,OK“ oder „02,REQUEST TRUNCATED“ gelten als gut
        cmp #'0'
        beq +
        lda #$e2
        jmp ende
+       lda #$a5
ende    sta MARK
        lda #0                  ; eigenes Programm „löschen“
        sta $0801
        sta $0802
        cli
        jmp READY

; ---------------------------------------------------------------- UCI

; Befehl schicken: X = Länge, A/Y = Adresse hoch/tief. Wartet lange (LOAD_REU von 16 MB).
uci_cmd stx tmp
        sta zp+1
        sty zp
        lda #10
        sta versuche
_v      jsr uci_frei
        ldy #0
-       lda (zp),y
        sta UCI+1
        iny
        cpy tmp
        bne -
        lda #1
        sta UCI
        lda UCI
        and #4
        beq _ang
        lda #8                  ; abgelehnt: Fehler löschen, kurz warten, wiederholen
        sta UCI
        ldx #0
        ldy #40
-       dex
        bne -
        dey
        bne -
        dec versuche
        bne _v
        lda #0
        sta stat_n
        rts
_ang    lda #0
        sta cnt
        sta cnt+1
        lda #$40                ; ~100 s bei 1 MHz
        sta cnt+2
-       lda UCI                 ; warten, bis Daten/Status bereitliegen (Zustand $20/$30) – direkt
        and #$20                ; nach dem Abschicken steht der Zustand oft noch kurz auf frei ($00)
        bne +
        jsr zaehl
        bne -
        lda #4                  ; Zeit abgelaufen: abbrechen
        sta UCI
        lda #0
        sta stat_n
        rts
+       ldy #0
-       lda UCI                 ; Daten verwerfen, Status sammeln
        bpl +
        lda UCI+2
        jmp -
+       lda UCI
        and #$40
        beq +
        lda UCI+3
        cpy #16
        bcs -
        sta stat_puf,y
        iny
        jmp -
+       sty stat_n
        lda #2                  ; annehmen
        sta UCI
-       lda UCI
        and #2
        bne -
        rts

; UCI in den Grundzustand bringen (hängende Antwort eines früheren Laufs abholen).
uci_reset
        ldx #0
_r      lda UCI
        and #$f0
        beq _ok
        lda UCI
        bpl +
        lda UCI+2
        jmp _r2
+       and #$40
        beq +
        lda UCI+3
        jmp _r2
+       lda #$02
        sta UCI
        lda UCI
        and #$30
        beq _r2
        lda #$04
        sta UCI
_r2     ldy #0
-       dey
        bne -
        dex
        bne _r
_ok     lda #$08
        sta UCI
        rts

uci_frei
        lda #0
        sta cnt
        sta cnt+1
        lda #$04
        sta cnt+2
-       lda UCI
        and #$30
        beq +
        jsr zaehl
        bne -
+       rts

zaehl   dec cnt
        bne +
        dec cnt+1
        bne +
        dec cnt+2
+       rts

; Status nach RES+X (16 Byte, mit Nullen aufgefüllt).
status_merken
        ldy #0
-       cpy stat_n
        bcs +
        lda stat_puf,y
        jmp _s
+       lda #0
_s      sta RES,x
        inx
        iny
        cpy #16
        bne -
        rts

versuche .byte 0
stat_n  .byte 0
stat_puf .fill 16, 0

c_close .byte $01, $03

; ---------------------------------------------------------------- Parameter (trägt Breadbin ein)
        .byte $42, $42, $52, $45, $55, $50, $41, $52   ; „BBREUPAR“ in ASCII
c_load  .byte $01, $21, 0, 0, 0, 0
len     .dword 0
plen    .byte 0
c_open  .byte $01, $02, $01
path    .fill 200, 0
