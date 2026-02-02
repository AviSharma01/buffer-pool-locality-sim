import json, sys, os
import imageio.v2 as imageio
import matplotlib.pyplot as plt

def load_snaps(run_dir):
    snap_dir = os.path.join(run_dir, "snapshots")
    files = sorted([f for f in os.listdir(snap_dir) if f.endswith(".json")])
    for f in files:
        with open(os.path.join(snap_dir, f), "r") as fh:
            yield json.load(fh)

def render_frame(snap, out_png):
    frames = snap["frames"]
    n = len(frames)
    side = int(n ** 0.5)

    grid = [[frames[r*side + c]["table_id"] for c in range(side)] for r in range(side)]

    plt.figure(figsize=(5,5))
    plt.imshow(grid, interpolation="nearest")
    plt.title(f"tick {snap['tick']}  recluster={snap['recluster']}")
    plt.axis("off")
    plt.tight_layout()
    plt.savefig(out_png, dpi=120)
    plt.close()

def main():
    run_dir = sys.argv[1]
    out_gif = os.path.join(run_dir, "run.gif")
    tmp_dir = os.path.join(run_dir, "_frames")
    os.makedirs(tmp_dir, exist_ok=True)

    pngs = []
    for i, snap in enumerate(load_snaps(run_dir)):
        out_png = os.path.join(tmp_dir, f"{i:06}.png")
        render_frame(snap, out_png)
        pngs.append(out_png)

    images = [imageio.imread(p) for p in pngs]
    imageio.mimsave(out_gif, images, duration=0.05)
    print("Wrote", out_gif)

if __name__ == "__main__":
    main()
