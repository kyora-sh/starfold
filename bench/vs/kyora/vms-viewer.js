// The live view on a sandbox's page: a small VNC client (RFB 3.8, security None, raw and copy-rect
// encodings, 32-bit pixels) over the page's WebSocket to the sandbox's x11vnc, drawn on a canvas in
// place of the refreshing screenshot. View only: nothing goes back but screen requests, and the
// machine drops anything else anyway. "take control" (its owner only, after a confirm) opens it again
// with control=1: then the canvas sends the mouse and keyboard, once the handshake is done, until
// pressed again or the view ends; whatever is still held goes up when the page loses focus.
// Under Next (apps/dashboard) it starts once React hydrated (app/_leaves/hydrated.tsx): earlier, React would redraw it away.
((start) => (globalThis.__next_f && !globalThis.kyoraHydrated ? globalThis.addEventListener("kyora:hydrated", start, { once: true }) : start()))(() => {
  const canvas = document.querySelector("canvas[data-live]")
  const toggle = document.querySelector("[data-live-toggle]")
  const take = document.querySelector("[data-live-control]")
  const image = document.querySelector("img[data-screen]")
  const mode = document.querySelector("[data-screen-mode]")
  const status = document.querySelector("[data-screen-status]")
  if (!canvas || !toggle || !take) return
  const path = canvas.getAttribute("data-live")
  if (!path || !path.startsWith("/")) return
  const context = canvas.getContext("2d")
  const idle = mode ? mode.textContent : ""
  const KEEPALIVE_MS = 20000

  const KEYS = {
    Backspace: 0xff08, Tab: 0xff09, Enter: 0xff0d, Escape: 0xff1b, Delete: 0xffff, Home: 0xff50, End: 0xff57,
    PageUp: 0xff55, PageDown: 0xff56, ArrowLeft: 0xff51, ArrowUp: 0xff52, ArrowRight: 0xff53, ArrowDown: 0xff54,
    Insert: 0xff63, ContextMenu: 0xff67, Shift: 0xffe1, Control: 0xffe3, Alt: 0xffe9, Meta: 0xffeb, CapsLock: 0xffe5,
  }
  const keysym = (event) => {
    if (KEYS[event.key] !== undefined) return KEYS[event.key]
    const f = /^F([1-9]|1[0-2])$/.exec(event.key)
    if (f) return 0xffbe + Number(f[1]) - 1
    if ([...event.key].length !== 1) return null
    const point = event.key.codePointAt(0)
    return point < 0x100 ? point : 0x01000000 + point
  }
  const u16 = (bytes, at) => (bytes[at] << 8) | bytes[at + 1]
  const u32 = (bytes, at) => ((bytes[at] << 24) >>> 0) + (bytes[at + 1] << 16) + (bytes[at + 2] << 8) + bytes[at + 3]
  const be16 = (n) => [(n >> 8) & 255, n & 255]
  const be32 = (n) => [(n >>> 24) & 255, (n >>> 16) & 255, (n >>> 8) & 255, n & 255]

  // the server's bytes, in the chunks they came in, taken off the front
  class Bytes {
    constructor() {
      this.chunks = []
      this.head = 0
      this.length = 0
    }
    push(chunk) {
      this.chunks.push(chunk)
      this.length += chunk.length
    }
    copy(n, consume) {
      const out = new Uint8Array(n)
      let at = 0
      let index = 0
      let head = this.head
      while (at < n) {
        const chunk = this.chunks[index]
        const take = Math.min(n - at, chunk.length - head)
        out.set(chunk.subarray(head, head + take), at)
        at += take
        head += take
        if (head === chunk.length) {
          index += 1
          head = 0
        }
      }
      if (consume) {
        this.chunks.splice(0, index)
        this.head = head
        this.length -= n
      }
      return out
    }
    peek(n) {
      return this.copy(n, false)
    }
    take(n) {
      return this.copy(n, true)
    }
  }

  const say = (text) => {
    if (status) status.textContent = text
  }

  let session = null

  function connect(control) {
    const scheme = location.protocol === "https:" ? "wss" : "ws"
    const ws = new WebSocket(`${scheme}://${location.host}${path}${control ? "?control=1" : ""}`)
    ws.binaryType = "arraybuffer"
    const queue = new Bytes()
    let stage = "version"
    let update = null
    // what this viewer pressed and has not let go yet: RFB button mask, keysyms, and where the pointer is
    let buttons = 0
    const keys = new Set()
    let at = { x: 0, y: 0 }
    let sentAt = Date.now()
    let ended = false
    const send = (bytes) => {
      if (ws.readyState !== WebSocket.OPEN) return
      ws.send(Uint8Array.from(bytes))
      sentAt = Date.now()
    }
    const request = (incremental) => send([3, incremental ? 1 : 0, 0, 0, 0, 0, ...be16(canvas.width), ...be16(canvas.height)])
    const fail = (why) => {
      say(why)
      ws.close(1002, why.slice(0, 120))
    }

    const rect = () => {
      if (!update.header) {
        if (queue.length < 12) return false
        const h = queue.take(12)
        update.header = { x: u16(h, 0), y: u16(h, 2), w: u16(h, 4), h: u16(h, 6), encoding: u32(h, 8) | 0 }
      }
      const { x, y, w, h, encoding } = update.header
      if (encoding === 0) {
        const size = w * h * 4
        if (queue.length < size) return false
        const pixels = queue.take(size)
        const data = context.createImageData(w, h)
        // the pixel format asked for: 32-bit little-endian, red at bit 16, green 8, blue 0
        for (let i = 0; i < w * h; i += 1) {
          data.data[4 * i] = pixels[4 * i + 2]
          data.data[4 * i + 1] = pixels[4 * i + 1]
          data.data[4 * i + 2] = pixels[4 * i]
          data.data[4 * i + 3] = 255
        }
        context.putImageData(data, x, y)
      } else if (encoding === 1) {
        if (queue.length < 4) return false
        const source = queue.take(4)
        context.drawImage(canvas, u16(source, 0), u16(source, 2), w, h, x, y, w, h)
      } else {
        fail(`the screen sent an encoding this viewer doesn't take (${encoding})`)
        return false
      }
      update.header = null
      update.left -= 1
      if (update.left === 0) {
        update = null
        request(true)
      }
      return true
    }

    const pump = () => {
      while (!ended) {
        if (stage === "version") {
          if (queue.length < 12) return
          const version = new TextDecoder().decode(queue.take(12))
          if (!version.startsWith("RFB 003.")) return fail("not a VNC server")
          send([...new TextEncoder().encode("RFB 003.008\n")])
          stage = "security"
        } else if (stage === "security") {
          if (queue.length < 1) return
          const count = queue.peek(1)[0]
          if (count === 0) return fail("the screen refused the connection")
          if (queue.length < 1 + count) return
          const types = queue.take(1 + count).subarray(1)
          if (!types.includes(1)) return fail("the screen asks for a password this viewer doesn't have")
          send([1])
          stage = "result"
        } else if (stage === "result") {
          if (queue.length < 4) return
          if (u32(queue.take(4), 0) !== 0) return fail("the screen refused the connection")
          send([1]) // ClientInit: shared
          stage = "init"
        } else if (stage === "init") {
          if (queue.length < 24) return
          const head = queue.peek(24)
          const name = u32(head, 20)
          if (queue.length < 24 + name) return
          queue.take(24 + name)
          canvas.width = u16(head, 0)
          canvas.height = u16(head, 2)
          send([0, 0, 0, 0, 32, 24, 0, 1, ...be16(255), ...be16(255), ...be16(255), 16, 8, 0, 0, 0, 0])
          send([2, 0, ...be16(2), ...be32(1), ...be32(0)])
          request(false)
          stage = "messages"
          // only now: input bytes earlier would land in the middle of the handshake
          if (control) enable()
          say("")
        } else if (update) {
          if (!rect()) return
        } else {
          if (queue.length < 1) return
          const type = queue.peek(1)[0]
          if (type === 0) {
            if (queue.length < 4) return
            update = { left: u16(queue.take(4), 2), header: null }
            if (update.left === 0) {
              update = null
              request(true)
            }
          } else if (type === 1) {
            if (queue.length < 6) return
            const colours = u16(queue.peek(6), 4)
            if (queue.length < 6 + 6 * colours) return
            queue.take(6 + 6 * colours)
          } else if (type === 2) {
            queue.take(1)
          } else if (type === 3) {
            if (queue.length < 8) return
            const length = u32(queue.peek(8), 4)
            if (length > 1048576) return fail("the screen sent too much clipboard text")
            if (queue.length < 8 + length) return
            queue.take(8 + length)
          } else return fail(`the screen sent a message this viewer doesn't know (${type})`)
        }
      }
    }

    ws.addEventListener("message", (event) => {
      if (typeof event.data === "string") return
      queue.push(new Uint8Array(event.data))
      pump()
    })
    const keepalive = setInterval(() => {
      // a quiet screen sends nothing: a request now and then keeps the connection from idling out
      if (stage === "messages" && Date.now() - sentAt > KEEPALIVE_MS) request(true)
    }, KEEPALIVE_MS / 4)

    // the mouse and keyboard, with control only and once the handshake is done (enable); the
    // view-only canvas has no handlers at all
    const pointer = (event) => {
      const box = canvas.getBoundingClientRect()
      const x = Math.max(0, Math.min(canvas.width - 1, Math.round(((event.clientX - box.left) * canvas.width) / box.width)))
      const y = Math.max(0, Math.min(canvas.height - 1, Math.round(((event.clientY - box.top) * canvas.height) / box.height)))
      return { x, y }
    }
    const mask = (event) => (event.buttons & 1 ? 1 : 0) | (event.buttons & 4 ? 2 : 0) | (event.buttons & 2 ? 4 : 0)
    const point = (event) => {
      at = pointer(event)
      send([5, buttons, ...be16(at.x), ...be16(at.y)])
    }
    // everything still down goes up: a key or button held as the page lost focus would stay held
    const release = () => {
      for (const sym of keys) send([4, 0, 0, 0, ...be32(sym)])
      keys.clear()
      if (buttons !== 0) {
        buttons = 0
        send([5, 0, ...be16(at.x), ...be16(at.y)])
      }
    }
    const onCanvas = {
      mousemove: (event) => {
        // the buttons as they are now, of those pressed here: one let go outside the canvas is up
        buttons &= mask(event)
        point(event)
      },
      mousedown: (event) => {
        event.preventDefault()
        canvas.focus()
        buttons = mask(event)
        point(event)
      },
      wheel: (event) => {
        event.preventDefault()
        const { x, y } = pointer(event)
        const bit = event.deltaY < 0 ? 8 : event.deltaY > 0 ? 16 : event.deltaX < 0 ? 32 : 64
        send([5, buttons | bit, ...be16(x), ...be16(y)])
        send([5, buttons, ...be16(x), ...be16(y)])
      },
      contextmenu: (event) => event.preventDefault(),
      keydown: (event) => {
        const sym = keysym(event)
        if (sym === null) return
        event.preventDefault()
        keys.add(sym)
        send([4, 1, 0, 0, ...be32(sym)])
      },
      keyup: (event) => {
        const sym = keysym(event)
        if (sym === null) return
        event.preventDefault()
        keys.delete(sym)
        send([4, 0, 0, 0, ...be32(sym)])
      },
      blur: release,
    }
    // a button pressed on the canvas goes up wherever it is let go
    const onDocument = {
      mouseup: (event) => {
        if (buttons === 0) return
        buttons &= mask(event)
        point(event)
      },
    }
    const onWindow = { blur: release }
    const listen = (target, handlers, on) => {
      for (const [name, handler] of Object.entries(handlers)) (on ? target.addEventListener : target.removeEventListener).call(target, name, handler)
    }
    let enabled = false
    const enable = () => {
      enabled = true
      listen(canvas, onCanvas, true)
      listen(document, onDocument, true)
      listen(window, onWindow, true)
    }

    const close = () => {
      if (ended) return
      ended = true
      clearInterval(keepalive)
      if (enabled) {
        release()
        listen(canvas, onCanvas, false)
        listen(document, onDocument, false)
        listen(window, onWindow, false)
      }
      if (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING) ws.close(1000, "the viewer closed it")
    }
    ws.addEventListener("close", (event) => {
      const was = session
      close()
      if (was && was.ws === ws) {
        session = null
        show(false)
        say(event.reason ? `live view ended: ${event.reason}` : "live view ended")
      }
    })
    return { ws, control, close }
  }

  // the screenshot or the live canvas, and the buttons that go with each
  const show = (live, control = false) => {
    canvas.hidden = !live
    if (image) image.hidden = live
    toggle.textContent = live ? "Stop live view" : "Live view"
    take.hidden = !live
    take.setAttribute("aria-pressed", control ? "true" : "false")
    take.textContent = control ? "Release control" : "Take control"
    canvas.classList.toggle("control", control)
    if (mode) {
      mode.textContent = !live ? idle : control ? "you are in control: the mouse and keyboard on the picture reach the sandbox" : "live, view only"
    }
  }
  const open = (control) => {
    if (session) {
      const old = session
      session = null
      old.close()
    }
    say("")
    session = connect(control)
    show(true, control)
  }

  toggle.addEventListener("click", () => {
    if (session) {
      const old = session
      session = null
      old.close()
      show(false)
      return
    }
    open(false)
  })
  take.addEventListener("click", () => {
    if (!session) return
    if (session.control) return open(false)
    if (!window.confirm("Take control of this sandbox's screen? Your mouse and keyboard on the picture will reach it, and kyora logs that you did.")) return
    open(true)
  })
})
