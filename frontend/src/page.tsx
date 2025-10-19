import { createRoot } from "react-dom/client";
import * as types from "./bindings/index";
import { useCallback, useState } from "react";

interface PageItemListRowProps {
  item: types.PageItem;
}
function PageItemListRow({ item }: PageItemListRowProps) {
  let className = item.kind == "Dir" ? "dir" : "file";

  let icon = item.kind == "Dir" ? <>📁</> : <>📃</>;
  if (item.thumbnail_data) {
    icon = <img src={"data:image/webp;base64," + item.thumbnail_data} />;
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
    <div className={`${className} row`}>
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
}
function PageItemList({ items, pathSep, fileDir, itemsInSubdirs, pageRoot }: PageItemListProps) {
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

  return (
    <>
      {searchWidget}

      <div className="header row">
        <div className="header" id="topleft"></div>
        {headerCols}
      </div>

      {filteredItems.map((item) => {
        return <PageItemListRow key={item.basename} item={item} />;
      })}

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

let root = document.getElementById("reactRoot")!;
createRoot(root).render(
  <PageItemList
    items={items}
    pathSep={pathSep}
    fileDir={fileDir}
    itemsInSubdirs={itemsInSubdirs}
    pageRoot={pageRoot}
  />,
);
