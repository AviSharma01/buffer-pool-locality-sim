import json, sys, os
import numpy as np
import matplotlib.pyplot as plt
import matplotlib.patches as mpatches
import matplotlib.gridspec as gridspec
import imageio.v2 as imageio

# ── colour palette ────────────────────────────────────────────────────────────
# one distinct colour per table_id; -1 (empty frame) → dark grey
TABLE_COLORS = [
    "#4E79A7", "#F28E2B", "#E15759", "#76B7B2", "#59A14F", "#EDC948",
]
EMPTY_COLOR  = "#2a2a2a"
BG_COLOR     = "#1a1a1a"
TEXT_COLOR   = "#e0e0e0"

def table_color(tid: int) -> tuple:
    """Return an (R,G,B) tuple in [0,1] for a table_id."""
    if tid < 0:
        hex_c = EMPTY_COLOR
    else:
        hex_c = TABLE_COLORS[tid % len(TABLE_COLORS)]
    hex_c = hex_c.lstrip("#")
    return tuple(int(hex_c[i:i+2], 16) / 255.0 for i in (0, 2, 4))


def load_snaps(run_dir: str):
    snap_dir = os.path.join(run_dir, "snapshots")
    files = sorted(f for f in os.listdir(snap_dir) if f.endswith(".json"))
    for f in files:
        with open(os.path.join(snap_dir, f)) as fh:
            yield json.load(fh)


# ── per-frame renderer ────────────────────────────────────────────────────────

def render_frame(snap: dict, history: dict, out_png: str) -> None:
    frames   = snap["frames"]
    metrics  = snap["metrics"]
    tick     = snap["tick"]
    recluster = snap["recluster"]
    n        = len(frames)
    side     = int(n ** 0.5)

    # Append metrics to rolling history
    history["ticks"].append(tick)
    history["hit_rate"].append(metrics["hit_rate"])
    history["avg_cost"].append(metrics["avg_cost"])
    history["locality_score"].append(metrics["locality_score"])

    # ── layout ────────────────────────────────────────────────────────────────
    fig = plt.figure(figsize=(12, 6), facecolor=BG_COLOR)
    gs  = gridspec.GridSpec(
        1, 2,
        width_ratios=[1.1, 1],
        left=0.04, right=0.97,
        top=0.84, bottom=0.06,
        wspace=0.12,
    )

    ax_grid  = fig.add_subplot(gs[0])
    ax_plots = fig.add_subplot(gs[1])

    # ── buffer-pool grid ──────────────────────────────────────────────────────
    cell = 1.0
    for idx, fr in enumerate(frames):
        row = idx // side
        col = idx  % side
        x   = col * cell
        y   = (side - 1 - row) * cell          # flip so row-0 is at top

        # background cell colour = table colour
        rect = mpatches.FancyBboxPatch(
            (x + 0.05, y + 0.05), cell - 0.10, cell - 0.10,
            boxstyle="round,pad=0.02",
            linewidth=0,
            facecolor=table_color(fr["table_id"]),
        )
        ax_grid.add_patch(rect)

        # dirty → white border
        if fr["dirty"]:
            border = mpatches.FancyBboxPatch(
                (x + 0.05, y + 0.05), cell - 0.10, cell - 0.10,
                boxstyle="round,pad=0.02",
                linewidth=1.8,
                edgecolor="#ffffff",
                facecolor="none",
            )
            ax_grid.add_patch(border)

        # ref_bit → small dot (top-right corner)
        if fr["ref_bit"]:
            dot = plt.Circle(
                (x + cell * 0.78, y + cell * 0.78),
                radius=cell * 0.10,
                color="#ffffff",
                zorder=3,
            )
            ax_grid.add_patch(dot)

    ax_grid.set_xlim(0, side)
    ax_grid.set_ylim(0, side)
    ax_grid.set_aspect("equal")
    ax_grid.axis("off")
    ax_grid.set_facecolor(BG_COLOR)

    # ── legend ────────────────────────────────────────────────────────────────
    legend_handles = [
        mpatches.Patch(facecolor=TABLE_COLORS[i], label=f"T{i}")
        for i in range(6)
    ]
    legend_handles.append(mpatches.Patch(facecolor=EMPTY_COLOR, label="empty"))
    ax_grid.legend(
        handles=legend_handles,
        loc="lower left", ncol=4,
        fontsize=7, framealpha=0.25,
        facecolor=BG_COLOR, labelcolor=TEXT_COLOR,
        bbox_to_anchor=(0, -0.08),
    )
    # dirty / ref_bit key (inline text)
    ax_grid.text(
        0.0, -0.05,
        "□ dirty   ● ref_bit",
        transform=ax_grid.transAxes,
        fontsize=7, color=TEXT_COLOR, va="top", ha="left",
    )

    # ── time-series plots ─────────────────────────────────────────────────────
    ticks_arr  = history["ticks"]
    plot_specs = [
        ("hit_rate",       "Hit rate",       "#4E79A7", (0.0, 1.0)),
        ("avg_cost",       "Avg cost (EMA)", "#E15759", None),
        ("locality_score", "Locality (↓ better)", "#59A14F", None),
    ]

    inner_gs = gridspec.GridSpecFromSubplotSpec(
        3, 1, subplot_spec=gs[1], hspace=0.55
    )
    fig.delaxes(ax_plots)

    for i, (key, label, color, ylim) in enumerate(plot_specs):
        ax = fig.add_subplot(inner_gs[i])
        ax.set_facecolor(BG_COLOR)
        ax.plot(ticks_arr, history[key], color=color, linewidth=1.2)
        ax.set_ylabel(label, fontsize=7, color=TEXT_COLOR)
        ax.tick_params(colors=TEXT_COLOR, labelsize=6)
        for spine in ax.spines.values():
            spine.set_edgecolor("#444444")
        if ylim:
            ax.set_ylim(*ylim)
        if i < 2:
            ax.set_xticklabels([])
        else:
            ax.set_xlabel("tick", fontsize=7, color=TEXT_COLOR)

        # mark recluster events with a vertical line
        for rt in history["recluster_ticks"]:
            ax.axvline(rt, color="#FFD700", linewidth=0.8, alpha=0.6)

    # ── title ─────────────────────────────────────────────────────────────────
    title = f"tick {tick}"
    if recluster:
        title += "   ★ RECLUSTER"
        history["recluster_ticks"].append(tick)

    fig.suptitle(
        title,
        fontsize=11, color="#FFD700" if recluster else TEXT_COLOR,
        fontweight="bold" if recluster else "normal",
        y=0.97,
    )

    order = snap.get("table_order", [])
    order_str = " → ".join(f"T{t}" for t in order)
    fig.text(
        0.5, 0.91,
        f"layout: {order_str}",
        fontsize=7.5, color="#aaaaaa", ha="center", va="top",
    )

    plt.savefig(out_png, dpi=110, facecolor=BG_COLOR)
    plt.close(fig)


# ── main ──────────────────────────────────────────────────────────────────────

def main():
    if len(sys.argv) < 2:
        print("usage: render.py <run_dir>")
        sys.exit(1)

    run_dir = sys.argv[1]
    out_gif = os.path.join(run_dir, "run.gif")
    tmp_dir = os.path.join(run_dir, "_frames")
    os.makedirs(tmp_dir, exist_ok=True)

    history = {
        "ticks":           [],
        "hit_rate":        [],
        "avg_cost":        [],
        "locality_score":  [],
        "recluster_ticks": [],
    }

    pngs = []
    snaps = list(load_snaps(run_dir))
    total = len(snaps)
    for i, snap in enumerate(snaps):
        out_png = os.path.join(tmp_dir, f"{i:06}.png")
        render_frame(snap, history, out_png)
        pngs.append(out_png)
        if (i + 1) % 50 == 0 or (i + 1) == total:
            print(f"  rendered {i+1}/{total}", end="\r", flush=True)

    print()
    images = [imageio.imread(p) for p in pngs]
    imageio.mimsave(out_gif, images, duration=0.05)
    print("Wrote", out_gif)


if __name__ == "__main__":
    main()
