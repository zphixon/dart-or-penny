import { createRoot } from "react-dom/client";
import * as types from "./bindings/index";
import { useInView } from "../node_modules/react-intersection-observer/dist/index";
import { useCallback, useEffect, useEffectEvent, useState } from "react";
import { useLocalStorage } from "usehooks-ts";

class ThumbnailEvent extends Event {
  response: types.ThumbnailResponse;
  constructor(response: types.ThumbnailResponse) {
    super("thumbnail");
    this.response = response;
  }
}

interface PageItemListRowProps {
  item: types.PageItem;
  target: EventTarget;
  socket: WebSocket | null;
}
function PageItemListRow({ item, target, socket }: PageItemListRowProps) {
  let className = item.kind == "Dir" ? "dir" : "file";

  let [thumbnail, setThumbnail] = useState<types.ThumbnailResponse | null>(
    null
  );
  useEffect(() => {
    if (item.thumbnail_filename) {
      target.addEventListener("thumbnail", (e) => {
        if (
          e instanceof ThumbnailEvent &&
          e.response.thumbnail === item.thumbnail_filename
        ) {
          setThumbnail(e.response);
        }
      });
    }
  }, [target]);

  let icon = item.kind == "Dir" ? <>📁</> : <>📃</>;
  if (thumbnail) {
    icon = <img src={"data:image/webp;base64," + thumbnail.data} />;
  } else if (item.thumbnail_filename) {
    // lol
    icon = (
      <svg
        width="24"
        height="24"
        viewBox="0 0 24 24"
        xmlns="http://www.w3.org/2000/svg"
      >
        <style>
          .spinner_P7sC{"{"}transform-origin:center;animation:spinner_svv2 .75s
          infinite linear{"}"}@keyframes spinner_svv2{"{"}100%{"{"}
          transform:rotate(360deg){"}"}
          {"}"}
        </style>
        <path
          d="M10.14,1.16a11,11,0,0,0-9,8.92A1.59,1.59,0,0,0,2.46,12,1.52,1.52,0,0,0,4.11,10.7a8,8,0,0,1,6.66-6.61A1.42,1.42,0,0,0,12,2.69h0A1.57,1.57,0,0,0,10.14,1.16Z"
          className="spinner_P7sC"
        />
      </svg>
    );
  }

  let link =
    item.kind == "Dir" ? (
      <a href={item.filename}>{item.basename}</a>
    ) : (
      <a href={item.filename} target="_blank" rel="noopener noreferrer">
        {item.basename}
      </a>
    );

  let { ref, inView } = useInView({ root: null, triggerOnce: true });
  if (inView && item.thumbnail_filename !== null && thumbnail === null) {
    socket?.send(item.thumbnail_filename);
  }

  return (
    <div ref={ref} className={`${className} row`}>
      <div className={`${className} icon`}>{icon}</div>
      <div className={`${className} filename`}>{link}</div>
      <div className={`${className} created`}>{item.created}</div>
      <div className={`${className} modified`}>{item.modified}</div>
      <div className={`${className} accessed`}>{item.accessed}</div>
    </div>
  );
}

interface PageItemListProps {
  pageRoot: string;
  csrfToken: string;
  items: types.PageItem[];
}
function PageItemList({ items, csrfToken }: PageItemListProps) {
  let [socket, setSocket] = useState<WebSocket | null>(null);
  let gotThumbnail = new EventTarget();

  useEffect(() => {
    let socket = new WebSocket(
      pageRoot + "/.dop/thumbnail?csrf=" + encodeURIComponent(csrfToken)
    );

    socket.onmessage = (event) => {
      let response: types.ThumbnailResponse | types.ThumbnailError = JSON.parse(
        event.data
      );
      if (response.type == "ThumbnailResponse") {
        window.localStorage.setItem(response.thumbnail, response.data);
        gotThumbnail.dispatchEvent(new ThumbnailEvent(response));
      } else {
        console.error(response);
      }
    };

    setSocket(socket);

    return () => socket.close();
  }, []);

  return (
    <>
      {items.map((item) => {
        return (
          <PageItemListRow
            key={item.basename}
            item={item}
            target={gotThumbnail}
            socket={socket}
          />
        );
      })}
    </>
  );
}

let pageRoot = document.getElementById("pageRoot")!.innerText;
let csrfToken = document.getElementById("csrfToken")!.innerText;

let itemsData = document.getElementById("items")!.innerText;
let items: types.PageItem[] = JSON.parse(itemsData);

let root = document.getElementById("page")!;
createRoot(root).render(
  <PageItemList items={items} pageRoot={pageRoot} csrfToken={csrfToken} />
);
