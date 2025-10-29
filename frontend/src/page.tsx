import { createRoot } from "react-dom/client";
import * as types from "./bindings/index";
import { useCallback, useEffect, useState } from "react";
import { useInView } from "../node_modules/react-intersection-observer/dist/index";

type ThumbnailResponse = types.ThumbnailData | types.ThumbnailError;

class ThumbnailEvent extends Event {
  response: ThumbnailResponse;
  constructor(response: ThumbnailResponse) {
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
  let { ref, inView } = useInView({ root: null, triggerOnce: true });
  let [thumbnailResponse, setThumbnailResponse] = useState<ThumbnailResponse | null>(
    item.thumbnail_name ? JSON.parse(window.localStorage.getItem(item.thumbnail_name) ?? "null") : null,
  );

  useEffect(() => {
    if (item.thumbnail_name) {
      target.addEventListener("thumbnail", (e) => {
        if (e instanceof ThumbnailEvent && e.response.name == item.thumbnail_name) {
          setThumbnailResponse(e.response);
        }
      });
    }
  }, [target]);

  if (inView && item.thumbnail_name !== null && thumbnailResponse === null) {
    // should be open, since we setSocket in the onopen handler
    socket?.send(item.thumbnail_name);
  }

  let className = item.kind == "Dir" ? "dir" : "file";
  let icon = item.kind == "Dir" ? <>📁</> : <>📃</>;
  if (thumbnailResponse !== null) {
    if (thumbnailResponse.type === "ThumbnailData") {
      icon = <img src={"data:image/webp;base64," + thumbnailResponse.data} />;
    } else if (thumbnailResponse.type === "ThumbnailError") {
      icon = <>❌</>;
      console.error("couldn't load thumbnail", thumbnailResponse);
    }
  } else if (item.thumbnail_name !== null) {
    // lol
    icon = (
      <svg width="24" height="24" viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg">
        <style>
          .spinner_P7sC{"{"}transform-origin:center;animation:spinner_svv2 .75s infinite linear{"}"}@keyframes
          spinner_svv2{"{"}100%{"{"}
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

type SortCol = "filename" | "created" | "modified" | "accessed";
type SortOrder = "dirsFirst" | "asc" | "desc";

interface PageItemListProps {
  items: types.PageItem[];
  pathSep: string;
  fileDir: string;
  itemsInSubdirs: number;
  pageRoot: string;
  csrf: string;
}
function PageItemList({ items, pathSep, fileDir, itemsInSubdirs, pageRoot, csrf }: PageItemListProps) {
  let [isSearchingEverywhere, setIsSearchingEverywhere] = useState(false);
  let [searchResults, setSearchResults] = useState<string[]>([]);
  let [caseSensitive, setCaseSensitive] = useState(false);
  let [searchInput, setSearchInput] = useState("");
  let [sortCol, setSortCol] = useState<SortCol>("filename");
  let [sortOrder, setSortOrder] = useState<SortOrder>("dirsFirst");
  let doSearchEverywhere = useCallback(() => {
    let path =
      pageRoot +
      "/.dop/search?regex=" +
      encodeURIComponent(searchInput) +
      "&" +
      (caseSensitive ? "case_insensitive=false" : "case_sensitive=true");

    fetch(path)
      .then((response) => response.json())
      .then(setSearchResults);
  }, [searchInput, caseSensitive]);

  let searchWidget = (
    <div id="searchboxdiv">
      <button
        id="clearSearch"
        onClick={(_) => {
          setSearchInput("");
          setIsSearchingEverywhere(false);
          setSearchResults([]);
          setCaseSensitive(false);
        }}
      >
        clear search
      </button>

      <input
        id="searchbox"
        type="text"
        placeholder="🔎 search"
        value={searchInput}
        onChange={(e) => {
          let newSearchInput = e.target.value;
          setSearchInput(newSearchInput);
          if (newSearchInput === "") {
            setIsSearchingEverywhere(false);
          }
        }}
        onKeyDown={(e) => {
          if (((!isSearchingEverywhere && e.shiftKey) || isSearchingEverywhere) && e.key === "Enter") {
            setIsSearchingEverywhere(true);
            doSearchEverywhere();
          }
          if (e.key === "Escape") {
            setSearchInput("");
            setIsSearchingEverywhere(false);
          }
        }}
      />

      <label id="caseSensitiveLabel" htmlFor="caseSensitive">
        case sensitive?
        <input
          id="caseSensitive"
          type="checkbox"
          checked={caseSensitive}
          onChange={(e) => setCaseSensitive(e.target.checked)}
        />
      </label>

      <button
        id="searchEverywhere"
        disabled={searchInput === ""}
        onClick={(_) => {
          setIsSearchingEverywhere(true);
          doSearchEverywhere();
        }}
      >
        {isSearchingEverywhere ? "🛰️ searching everywhere" : "search everywhere"}
      </button>
    </div>
  );

  let noResults = <div className="centerme">no results {isSearchingEverywhere ? "anywhere 🛰️" : ""}</div>;
  if (items.length === 0) {
    noResults = <div className="centerme">nothing here</div>;
  }

  let logoThing = (
    <div className="centerme">
      <img id="logoThing" src={pageRoot + "/.dop/assets/apple-touch-icon.png"} />
    </div>
  );

  if (isSearchingEverywhere) {
    return (
      <>
        {searchWidget}

        {searchResults.length === 0 ? noResults : ""}

        {searchResults.map((result, i) => {
          let parts = result.split(pathSep);

          let href = pageRoot;
          let as = [<a href={href}>{fileDir}</a>];
          for (let part of parts) {
            href += "/" + part;
            as.push(<a href={href}>{part}</a>);
          }

          let combined = as.reduce((resultLinks, a) => (
            <>
              {resultLinks} {pathSep} {a}
            </>
          ));

          return (
            <div key={i} className="everywheresearch">
              {combined}
            </div>
          );
        })}

        {logoThing}
      </>
    );
  }

  function doSort(oldItems: types.PageItem[]): types.PageItem[] {
    if (sortOrder === "dirsFirst") {
      return items;
    }
    if (sortCol === "filename") {
      if (sortOrder === "asc") {
        return [...oldItems].sort((a, b) => a[sortCol].toUpperCase().localeCompare(b[sortCol].toUpperCase()));
      }
      if (sortOrder === "desc") {
        return [...oldItems].sort(
          (a, b) => -a[sortCol].toUpperCase().localeCompare(b[sortCol].toUpperCase()),
        );
      }
    } else {
      if (sortOrder === "asc") {
        return [...oldItems].sort((a, b) => new Date(a[sortCol]).getTime() - new Date(b[sortCol]).getTime());
      }
      if (sortOrder === "desc") {
        return [...oldItems].sort((a, b) => new Date(b[sortCol]).getTime() - new Date(a[sortCol]).getTime());
      }
    }
    console.error("unexpected value for orderBy", sortOrder);
    return [];
  }
  function nextSortOrder(sortCol: SortCol, sortOrder: SortOrder): SortOrder {
    if (sortCol === "filename") {
      if (sortOrder === "dirsFirst") return "asc";
      if (sortOrder === "asc") return "desc";
      if (sortOrder === "desc") return "dirsFirst";
    }
    if (sortOrder === "dirsFirst") return "desc";
    if (sortOrder === "desc") return "asc";
    if (sortOrder === "asc") return "dirsFirst";
    console.error("unexpected value for orderBy", sortOrder);
    return "dirsFirst";
  }

  let filteredItems = doSort(items);
  if (searchInput !== "") {
    let flags = caseSensitive ? "" : "i";
    filteredItems = filteredItems.filter((item) => item.filename.search(new RegExp(searchInput, flags)) >= 0);
  }

  let headers: SortCol[] = ["filename", "created", "modified", "accessed"];
  let headerCols = headers.map((col, i) => (
    <div
      key={i}
      className={"header " + col + (col === sortCol ? " " + sortOrder : "")}
      onClick={(_) => {
        if (col === sortCol) {
          setSortOrder(nextSortOrder(col, sortOrder));
        } else {
          setSortCol(col);
          setSortOrder(nextSortOrder(col, "dirsFirst"));
        }
      }}
    >
      {col}
    </div>
  ));

  let numDirs = items.filter((item) => item.kind === "Dir").length;

  let gotThumbnail = new EventTarget();
  let [socket, setSocket] = useState<WebSocket | null>(null);
  useEffect(() => {
    let socket = new WebSocket(pageRoot + "/.dop/thumbnail?csrf=" + encodeURIComponent(csrf));

    socket.onmessage = (event) => {
      let response: ThumbnailResponse = JSON.parse(event.data);
      if (response.type === "ThumbnailData") {
        window.localStorage.setItem(response.name, JSON.stringify(response));
      }
      gotThumbnail.dispatchEvent(new ThumbnailEvent(response));
    };

    socket.onopen = (_) => setSocket(socket);

    return () => socket.close();
  }, []);

  return (
    <>
      {searchWidget}

      <div className="header row">
        <div className="header" id="topleft"></div>
        {headerCols}
      </div>

      {filteredItems.map((item) => (
        <PageItemListRow key={item.basename} item={item} target={gotThumbnail} socket={socket} />
      ))}

      <div id="numfiles">
        {searchInput !== "" ? <>{filteredItems.length} of</> : ""}
        {items.length}
        {numDirs !== 0 ? <>/ {itemsInSubdirs} </> : ""}
      </div>

      {filteredItems.length === 0 ? noResults : ""}

      {logoThing}
    </>
  );
}

let items: types.PageItem[] = JSON.parse(document.getElementById("items")!.innerText);
let pathSep = document.getElementById("pathSep")!.innerText;
let fileDir = document.getElementById("fileDir")!.innerText;
let itemsInSubdirs = parseInt(document.getElementById("itemsInSubdirs")!.innerText);
let pageRoot = document.getElementById("pageRoot")!.innerText;
let csrf = document.getElementById("csrf")!.innerText;

let root = document.getElementById("reactRoot")!;
createRoot(root).render(
  <PageItemList
    items={items}
    pathSep={pathSep}
    fileDir={fileDir}
    itemsInSubdirs={itemsInSubdirs}
    pageRoot={pageRoot}
    csrf={csrf}
  />,
);
