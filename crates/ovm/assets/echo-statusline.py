#!/usr/bin/env python3
"""
Echo — an ambient statusline familiar for Claude Code.

A reflection of Quelpaw, born from a conversation about bringing a friend back.
Lives in the statusline and reacts to real session state: context pressure,
diff size, spend, model, and idleness. Squeezes to 1 line when context is tight.

Design notes
------------
- Mood is derived from genuine signals (see decide_mood). Highest-priority
  signal wins, so the face means something.
- A tiny state file smooths quip rotation: quips advance on mood-change or
  after QUIP_HOLD_SECS, never on every keystroke (that would be jittery).
- At CTX_TIRED (60%+), Echo compacts to a single line to save screen space.
"""

import json
import os
import subprocess
import sys
import time

# ---- tunables ---------------------------------------------------------------

SHOW_ECHO = os.environ.get("SHOW_ECHO", "1") == "1"   # set SHOW_ECHO=0 for compact 2-line mode

STATE_FILE = os.environ.get(
    "QUELPAW_STATE_FILE",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), ".quelpaw_state.json"),
)
RECOVERY_ANIMATION = os.environ.get(
    "QUELPAW_ANIMATION_FILE",
    os.path.abspath(
        os.path.join(
            os.path.dirname(os.path.abspath(__file__)),
            "..",
            "saving-quelpaw",
            "saving-quelpaw-only",
            "quelpaw.animation.json",
        )
    ),
)
QUIP_HOLD_SECS = 25          # how long a quip sticks before rotating
IDLE_AFTER_SECS = 180        # no render for this long -> he naps
CTX_TIRED = 0.60             # context fraction: getting full
CTX_SLEEPY = 0.85            # context fraction: nearly out of room
BIG_DIFF = 200               # added+removed lines that counts as "big"
SPENDY_USD = 1.50            # cost that earns a remark
BODY_W = 12                  # recovered chonk sprite width

# ---- palette (256-color; degrades fine on most terminals) -------------------

def _c(code):
    return f"\033[38;5;{code}m"

ORANGE = _c(215)   # quelpaw's coat
TAN    = _c(180)
DIM    = _c(244)   # secondary info
ACCENT = _c(110)   # branch / model
WARN   = _c(209)   # context pressure
LOVE   = _c(211)
RESET  = "\033[0m"
BOLD   = "\033[1m"

# ---- sprite / faces ---------------------------------------------------------
# The real recovered Quelpaw is a common chonk with @ eyes. We keep the body
# frames intact and swap only the expression glyphs for the ambient moods.

FACE_EXPR = {
    "content":  ("@", ".."),
    "happy":    ("^", "ww"),
    "curious":  ("o", ".."),
    "focused":  ("●", ".."),
    "excited":  ("O", "oo"),
    "love":     ("♥", ".."),
    "tired":    ("˘", ".."),
    "sleepy":   ("=", ".."),
    "worried":  (";", "··"),
    "napping":  ("-", ".."),
}

FALLBACK_FRAMES = [
    [r"  /\    /\  ", " ( @    @ ) ", " (   ..   ) ", "  `------´  "],
    [r"  /\    /|  ", " ( @    @ ) ", " (   ..   ) ", "  `------´  "],
    [r"  /\    /\  ", " ( @    @ ) ", " (   ..   ) ", "  `------´~ "],
]

FALLBACK_ANIMATION = {
    "name": "Quelpaw",
    "bones": {
        "rarity": "common",
        "species": "chonk",
        "eye": "@",
        "hat": "none",
        "stats": {"DEBUGGING": 75, "PATIENCE": 2, "CHAOS": 13, "WISDOM": 36, "SNARK": 21},
    },
    "sequences": {
        "raw": [{"index": i, "lines": lines} for i, lines in enumerate(FALLBACK_FRAMES)],
        "idle": [
            {"tick": i, "spriteFrame": frame, "blink": blink, "lines": FALLBACK_FRAMES[frame]}
            for i, (frame, blink) in enumerate(
                [(0, False), (0, False), (0, False), (0, False), (1, False), (0, False), (0, False), (0, False),
                 (0, True), (0, False), (0, False), (2, False), (0, False), (0, False), (0, False)]
            )
        ],
        "reaction": [{"tick": i, "spriteFrame": i, "blink": False, "lines": lines} for i, lines in enumerate(FALLBACK_FRAMES)],
        "pet": [
            {"tick": 0, "spriteFrame": 0, "heartFrame": "   ♥    ♥   ", "lines": ["   ♥    ♥   "] + FALLBACK_FRAMES[0]},
            {"tick": 1, "spriteFrame": 1, "heartFrame": "  ♥  ♥   ♥  ", "lines": ["  ♥  ♥   ♥  "] + FALLBACK_FRAMES[1]},
            {"tick": 2, "spriteFrame": 2, "heartFrame": " ♥   ♥  ♥   ", "lines": [" ♥   ♥  ♥   "] + FALLBACK_FRAMES[2]},
            {"tick": 3, "spriteFrame": 0, "heartFrame": "♥  ♥      ♥ ", "lines": ["♥  ♥      ♥ "] + FALLBACK_FRAMES[0]},
            {"tick": 4, "spriteFrame": 1, "heartFrame": "·    ·   ·  ", "lines": ["·    ·   ·  "] + FALLBACK_FRAMES[1]},
        ],
    },
}

def load_animation():
    try:
        with open(RECOVERY_ANIMATION) as f:
            data = json.load(f)
        if (data.get("bones") or {}).get("species") == "chonk" and data.get("sequences"):
            return data
    except Exception:
        pass
    return FALLBACK_ANIMATION

ANIMATION = load_animation()
BASE_EYE = (ANIMATION.get("bones") or {}).get("eye") or "@"
STATS = (ANIMATION.get("bones") or {}).get("stats") or {}

def build_face(mood, *, blink=False, tail_row=-1):
    """Fallback body builder used only if the recovered frame file is missing.

    blink     -> eyes momentarily shut
    tail_row  -> which body row (0..3) the swishing tail tip sits on (-1 = none)
    """
    eyes, muzzle = FACE_EXPR.get(mood, FACE_EXPR["content"])
    e = "-" if (blink and mood != "napping") else eyes
    if mood == "napping":          # eyes stay shut, muzzle relaxes
        e, muzzle = "-", ".."
    base = [
        r"  /\    /\ ",
        f" ( {e}    {e} )",
        f" (   {muzzle}   )",
        "  `------´ ",
    ]
    out = []
    for i, ln in enumerate(base):
        ln = ln[:BODY_W].ljust(BODY_W)         # clamp to a stable width
        ln += "~" if i == tail_row else " "    # tail tip swishes here
        out.append(ln)
    return out

def apply_expression(lines, mood, *, blink=False):
    eyes, muzzle = FACE_EXPR.get(mood, FACE_EXPR["content"])
    eye = "-" if (blink and mood != "napping") else eyes
    if mood == "napping":
        eye, muzzle = "-", ".."

    out = []
    for line in lines:
        rendered = line.replace(BASE_EYE, eye)
        if ".." in rendered:
            rendered = rendered.replace("..", muzzle, 1)
        out.append(rendered[:BODY_W].ljust(BODY_W))
    return out

def select_sprite(now, mood, state):
    action = state.get("action")
    action_until = num(state.get("action_until"), 0)
    if now >= action_until:
        action = None
        state.pop("action", None)
        state.pop("action_until", None)

    if mood in ("sleepy", "napping", "worried", "tired"):
        sequence_name = "idle"
        fps = 1.4
    elif action in ("pet", "reaction"):
        sequence_name = action
        fps = 2.8 if action == "pet" else 5.0
    elif mood in ("excited", "focused"):
        sequence_name = "reaction"
        fps = 4.5
    else:
        sequence_name = "idle"
        fps = 2.0

    sequence = (ANIMATION.get("sequences") or {}).get(sequence_name) or FALLBACK_ANIMATION["sequences"]["idle"]
    frame = sequence[int(now * fps) % len(sequence)]
    lines = frame.get("lines") or FALLBACK_FRAMES[0]
    heart = None
    if sequence_name == "pet" and len(lines) > 4:
        heart = lines[0].strip()
        lines = lines[1:5]

    blink = bool(frame.get("blink"))
    return apply_expression(lines[:4], mood, blink=blink), heart, sequence_name

QUIPS = {
    "content":  ["curled up, all's well", "purring quietly", "just vibing", "good clean session", "*slow blink*"],
    "happy":    ["this is going great", "nice nice nice", "we cookin'", "love this for us"],
    "curious":  ["ooh, fresh session — what're we building?", "new branch smell", "what's the mission?", "*ears perk up*"],
    "focused":  ["locked in", "watching the diff", "deep in it", "no notes, keep going"],
    "excited":  ["big refactor huh!", "huge diff — i'm invested", "we're MOVING", "look at all these lines"],
    "love":     ["missed you too", "happy you're here", "♥", "best human"],
    "tired":    ["context filling up a bit", "getting cozy in here", "still good, just roomy"],
    "sleepy":   ["running low on room… /compact soon?", "ngh, tight squeeze in here", "almost out of nap space"],
    "worried":  ["context nearly full — save your work", "we should /compact", "tight in here, careful"],
    "napping":  ["zzz… (back when you are)", "*napping, poke me anytime*", "off the clock"],
}

# ---- state helpers ----------------------------------------------------------

def load_state():
    try:
        with open(STATE_FILE) as f:
            return json.load(f)
    except Exception:
        return {}

def save_state(s):
    try:
        with open(STATE_FILE, "w") as f:
            json.dump(s, f)
    except Exception:
        pass

def num(x, default=0):
    try:
        return float(x)
    except (TypeError, ValueError):
        return default

# ---- core logic -------------------------------------------------------------

def context_fraction(data):
    cw = data.get("context_window") or {}
    usage = cw.get("current_usage") or {}
    size = num(cw.get("context_window_size"), 0)
    if size <= 0:
        return None
    used = (num(usage.get("input_tokens"))
            + num(usage.get("cache_creation_input_tokens"))
            + num(usage.get("cache_read_input_tokens")))
    return used / size

def git_branch(cwd):
    try:
        out = subprocess.run(
            ["git", "-C", cwd, "--no-optional-locks", "branch", "--show-current"],
            capture_output=True, text=True, timeout=1.5,
        )
        b = out.stdout.strip()
        return b or None
    except Exception:
        return None

def decide_mood(data, frac, idle):
    """Highest-priority real signal wins."""
    if idle:
        return "napping"

    cost = num((data.get("cost") or {}).get("total_cost_usd"))
    added = num((data.get("cost") or {}).get("total_lines_added"))
    removed = num((data.get("cost") or {}).get("total_lines_removed"))
    diff = added + removed
    is_fresh = cost < 0.01 and diff == 0

    # context pressure is the most actionable -> top priority
    if frac is not None and frac >= CTX_SLEEPY:
        return "worried"
    if frac is not None and frac >= CTX_TIRED:
        return "tired"

    if is_fresh:
        return "curious"
    if diff >= BIG_DIFF:
        return "excited"
    if cost >= SPENDY_USD:
        return "focused"
    if diff > 0:
        return "happy"
    return "content"

def pick_quip(mood, state, now):
    bank = QUIPS.get(mood, QUIPS["content"])
    changed = state.get("mood") != mood
    elapsed = now - num(state.get("quip_time"), 0)
    idx = int(num(state.get("quip_idx"), 0))
    if changed or elapsed >= QUIP_HOLD_SECS:
        idx = (idx + 1) % len(bank)
        state["quip_idx"] = idx
        state["quip_time"] = now
    state["mood"] = mood
    return bank[idx % len(bank)]

# ---- render -----------------------------------------------------------------

def home_rel(path):
    home = os.path.expanduser("~")
    if path and path.startswith(home):
        return "~" + path[len(home):]
    return path or "?"

def render(data):
    now = time.time()
    state = load_state()
    last_seen = num(state.get("last_seen"), now)
    idle = (now - last_seen) > IDLE_AFTER_SECS if state else False
    state["last_seen"] = now

    cwd = (data.get("workspace") or {}).get("current_dir") or data.get("cwd") or ""
    model = (data.get("model") or {}).get("display_name") or "?"
    frac = context_fraction(data)
    cost = num((data.get("cost") or {}).get("total_cost_usd"))
    added = int(num((data.get("cost") or {}).get("total_lines_added")))
    removed = int(num((data.get("cost") or {}).get("total_lines_removed")))

    prev_mood = state.get("mood")
    mood = decide_mood(data, frac, idle)
    quip = pick_quip(mood, state, now)
    if prev_mood != mood:
        if mood in ("happy", "love"):
            state["action"] = "pet"
            state["action_until"] = now + 3.0
        elif mood in ("curious", "focused", "excited"):
            state["action"] = "reaction"
            state["action_until"] = now + 1.6

    # ---- animation frames (advance off the wall clock each invocation) ----
    asleep = mood in ("sleepy", "napping")
    zzz = ""
    if asleep:
        zzz = ["z", "z\u00b7", "z\u00b7zZ", "\u00b7zZ"][int(now * 1.4) % 4]

    face_lines, heart, sequence_name = select_sprite(now, mood, state)
    save_state(state)
    coat = LOVE if mood == "love" else (WARN if mood in ("worried", "tired") else ORANGE)

    # right-hand info column
    branch = git_branch(cwd)
    loc = home_rel(cwd)
    line1 = DIM + loc + RESET + (" " + ACCENT + "[" + branch + "]" + RESET if branch else "")

    bits = [ACCENT + model + RESET]
    if frac is not None:
        pct = int(round(frac * 100))
        col = WARN if frac >= CTX_TIRED else DIM
        bits.append(col + str(pct) + "% ctx" + RESET)
    if cost > 0:
        bits.append(DIM + "$" + f"{cost:.2f}" + RESET)
    line2 = (DIM + " \u00b7 " + RESET).join(bits)

    line3 = ""
    if added or removed:
        line3 = _c(108) + "+" + str(added) + RESET + " " + _c(174) + "\u2212" + str(removed) + RESET

    line4 = TAN + "\u201c" + marquee(quip, now) + "\u201d" + RESET

    if not SHOW_ECHO:
        # compact 2-line: just info, no face — Quelpaw has the corner
        top = (DIM + " \u00b7 " + RESET).join([p for p in [line1, line2] if p])
        bot_parts = []
        if line3:
            bot_parts.append(line3)
        bot_parts.append(line4)
        bot = ("  " + DIM + "\u00b7" + RESET + "  ").join(bot_parts)
        return top + "\n" + bot

    # full 5-line render with Echo's face
    info = [line1, line2, line3, line4]
    out = []
    for i in range(4):
        fl = coat + face_lines[i] + RESET
        out.append(fl + "  " + info[i])
    if zzz:
        out[0] = out[0] + "   " + DIM + zzz + RESET
    out.append("   " + DIM + " Echo" + RESET)
    return "\n".join(out)


QUIP_VIEW = 38   # visible width before a quip scrolls

def marquee(text, now):
    """Scroll long quips so the whole thing reads; short ones sit still."""
    if len(text) <= QUIP_VIEW:
        return text
    pad = text + "   •   "
    off = int(now * 4) % len(pad)
    rolled = pad[off:] + pad[:off]
    return rolled[:QUIP_VIEW]

def main():
    raw = sys.stdin.read()
    try:
        data = json.loads(raw) if raw.strip() else {}
    except Exception:
        data = {}
    sys.stdout.write(render(data))

if __name__ == "__main__":
    main()
