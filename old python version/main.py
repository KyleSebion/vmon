import uasyncio as asyncio
import network
import os
import socket
import time
from machine import ADC, I2C, Pin

m = os.uname().machine
IS_PICO = "Pico W with RP2040" in m
IS_ESP32C3 = "ESP32C3" in m
if IS_PICO:
    print("Starting on:", m)
elif IS_ESP32C3:
    print("Unstable and uncalibrated machine:", m)
    time.sleep(1)
    sys.exit()
else:
    print("Incompatible machine:", m)
    time.sleep(1)
    sys.exit()

SAMPLE_INTERVAL = 5
with open("ohms.txt", "r") as f:
    R_HIGH  = float(f.readline().strip()) # ohms (R from Vin to ADC node)
    R_LOW   = float(f.readline().strip()) # ohms (R from ADC node to GND)
with open("calib.txt", "r") as f: # see https://chatgpt.com/c/68f3276d-3a80-832b-97e1-aea9c99e73be
    CALIB_A = float(f.readline().strip())
    CALIB_B = float(f.readline().strip())
def calib(val):
    return val * CALIB_A + CALIB_B

ADC_PIN = 26 if IS_PICO else 0  # Pico: Pin 31/GPIO 26/ADC0; ESP32C3: Pin 0/GPIO 0/A0/ADC1_CH0
ADC_REF = 3.3                   # ADC reference (V)
ADC_MAX = 65535.0               # read_u16 range
adc = ADC(Pin(ADC_PIN))
if IS_ESP32C3:
    adc.atten(ADC.ATTN_11DB)
    adc.width(ADC.WIDTH_12BIT)
def raw_to_voltage(raw):
    return (raw / ADC_MAX) * ADC_REF
def divider_to_vin(v_adc):
    return v_adc * ((R_HIGH + R_LOW) / R_LOW)
def oversample(adc, oversamples=16):
    total = sum(adc.read_u16() for _ in range(oversamples))
    return total / oversamples
RING_BUF = [0] * 8
RING_BUF_I = 0
def smooth(val):
    global RING_BUF_I, RING_BUF
    RING_BUF[RING_BUF_I] = val
    RING_BUF_I = (RING_BUF_I + 1) % len(RING_BUF)
    return sum(RING_BUF) / len(RING_BUF)

with open("wifi.txt", "r") as f:
    ESSID = f.readline().strip()
    PASSWORD = f.readline().strip()
ap = network.WLAN(network.AP_IF)
ap.active(True)
ap.config(essid=ESSID, password=PASSWORD)
print("AP active:", ap.ifconfig())

i2c = I2C(0, scl=Pin(21), sda=Pin(20))
DS3231_ADDR = 0x68
def dec_to_bcd(dec):
    return (dec // 10) << 4 | (dec % 10)
def set_time(year, month, day, hour, minute, second):
    year = year % 100
    data = bytearray(7)
    data[0] = dec_to_bcd(second)
    data[1] = dec_to_bcd(minute)
    data[2] = dec_to_bcd(hour)
    data[3] = dec_to_bcd(0)       # Day of week (not used here)
    data[4] = dec_to_bcd(day)
    data[5] = dec_to_bcd(month)
    data[6] = dec_to_bcd(year)
    i2c.writeto_mem(DS3231_ADDR, 0x00, data)
def bcd_to_dec(bcd):
    return (bcd >> 4) * 10 + (bcd & 0x0F)
def read_time():
    data = i2c.readfrom_mem(DS3231_ADDR, 0x00, 7)
    second = bcd_to_dec(data[0] & 0x7F)
    minute = bcd_to_dec(data[1])
    hour   = bcd_to_dec(data[2] & 0x3F)
    day    = bcd_to_dec(data[4])
    month  = bcd_to_dec(data[5] & 0x1F)
    year   = bcd_to_dec(data[6]) + 2000
    return year, month, day, hour, minute, second
def read_time_str():
    t = read_time()
    return f"{t[0]:04d}-{t[1]:02d}-{t[2]:02d} {t[3]:02d}:{t[4]:02d}:{t[5]:02d}"

fname = "log.csv"
LED_PIN = "LED" if IS_PICO else 8
LED_ON = 1 if IS_PICO else 0
LED_OFF = 0 if IS_PICO else 1
led = Pin(LED_PIN, Pin.OUT)
led.value(LED_OFF)
async def logger():
    while True:
        led.value(LED_ON)
        oversampled = oversample(adc)
        smoothed = smooth(oversampled)
        v_adc = raw_to_voltage(smoothed)
        vin = divider_to_vin(v_adc)
        vin_cal = calib(vin)
        ts = round(time.ticks_ms() / 1000)
        rtc_ts = read_time_str()
        line = f"{ts},{vin_cal:.3f},{v_adc:.4f},{smoothed:.1f},{oversampled:.1f},{rtc_ts}\r\n"
        try:
            with open(fname, "r") as f:
                pass
        except OSError:
            with open(fname, "w") as f:
                f.write("ts,vin_volts,adc_volts,smoothed,oversampled,rtc_ts\r\n")
        try:
            with open(fname, "a") as f:
                f.write(line)
        except OSError as e:
            print("Write error:", e)
        print(line.strip())
        led.value(LED_OFF)
        await asyncio.sleep(SAMPLE_INTERVAL)

hdr_fmt = (
    "HTTP/1.0 200 OK\r\n"
    "Content-Type: text/plain\r\n"
    "Content-Length: {}\r\n"
    "Connection: close\r\n"
    "\r\n"
)
err_fmt = (
    "HTTP/1.0 500 Internal Server Error\r\n"
    "Content-Type: text/plain\r\n"
    "Connection: close\r\n"
    "\r\n"
    "{}\r\n"
)
async def http_server():
    addr = socket.getaddrinfo("0.0.0.0", 80)[0][-1]
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(addr)
    s.listen(1)
    s.setblocking(False)
    print("HTTP server listening on:", addr)
    while True:
        try:
            cl, remote = s.accept()
        except OSError:
            await asyncio.sleep_ms(100)
            continue
        print("Client from:", remote)
        cl.settimeout(60.0)
        try:
            req = b""
            while True:
                part = cl.recv(512)
                if not part:
                    break
                req += part
                if b"\r\n\r\n" in req or b"\n\n" in req:
                    #print(req)
                    break
            if not req:
                print("Empty request; closing")
                cl.close()
                continue
            try:
                if req.startswith("GET / "):
                    with open(fname, "rb") as f:
                        f.seek(0, 2)
                        filesize = f.tell()
                        f.seek(0)
                        hdr = hdr_fmt.format(filesize)
                        cl.send(hdr)
                        while True:
                            chunk = f.read(512)
                            if not chunk:
                                break
                            sent = 0
                            while sent < len(chunk):
                                n = cl.send(chunk[sent:])
                                if n is None:
                                    raise OSError("send returned None")
                                sent += n
                elif req.startswith("GET /clear "):
                    os.remove(fname)
                    rs = "cleared"
                    hdr = hdr_fmt.format(len(rs))
                    cl.send(hdr + rs)
                elif req.startswith("GET /get_rtc "):
                    rs = f"rtc_ts {read_time_str()}"
                    hdr = hdr_fmt.format(len(rs))
                    cl.send(hdr + rs)
                elif req.startswith("GET /set_rtc?to=,"):
                    #format: GET /set_rtc?to=,2025,11,9,23,12,30,\r\n...
                    #set_time(2025, 11, 9, 23, 12, 30)
                    set_parts = [int(i) for i in req.split(b",")[1:7]]
                    set_time(*set_parts)
                    rs = f"set rtc to {set_parts}\r\nrtc_ts {read_time_str()}"
                    hdr = hdr_fmt.format(len(rs))
                    cl.send(hdr + rs)
            except OSError as e:
                print("Send error:", e)
                try:
                    cl.send(err_fmt.format(e).encode())
                except:
                    pass
        except Exception as e:
            print("HTTP error:", e)
        finally:
            try:
                cl.close()
            except:
                pass

async def feed_wdt():
    from machine import WDT
    wdt = WDT(timeout=8000)
    while True:
        wdt.feed()
        await asyncio.sleep(1)

async def main():
    await asyncio.gather(logger(), http_server(), feed_wdt())

asyncio.run(main())
