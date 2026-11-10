import { useEffect, useState } from "react";
import { getFileAssetUrl, IMAGE_ASSETS_CHANGED_EVENT } from "../../api/tauri";
import type { DatasetImage } from "../../types/domain";

export function useImageAssetUrls(projectId: string | undefined, images: DatasetImage[], limit = 12) {
  const [urls, setUrls] = useState<Record<string, string>>({});
  const [generation, setGeneration] = useState(0);

  useEffect(() => {
    const invalidate = () => setGeneration((value) => value + 1);
    window.addEventListener(IMAGE_ASSETS_CHANGED_EVENT, invalidate);
    return () => window.removeEventListener(IMAGE_ASSETS_CHANGED_EVENT, invalidate);
  }, []);

  useEffect(() => {
    if (!projectId || images.length === 0) {
      setUrls({});
      return;
    }
    let cancelled = false;
    Promise.all(images.slice(0, limit).map(async (image) => {
      try {
        return [image.id, await getFileAssetUrl(projectId, image.id)] as const;
      } catch {
        return null;
      }
    })).then((entries) => {
      if (!cancelled) setUrls(Object.fromEntries(entries.filter((entry): entry is readonly [string, string] => Boolean(entry))));
    });
    return () => { cancelled = true; };
  }, [images, limit, projectId, generation]);

  return urls;
}
