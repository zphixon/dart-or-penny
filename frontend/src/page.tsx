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

interface PageItemListProps {
  items: types.PageItem[];
  pathSep: string;
  fileDir: string;
  numFiles: number;
  pageRoot: string;
}
function PageItemList({
  items,
  pathSep,
  fileDir,
  numFiles,
  pageRoot,
}: PageItemListProps) {
  let [isSearchingEverywhere, setIsSearchingEverywhere] = useState(false);
  let [searchResults, setSearchResults] = useState<string[]>([]);
  let [caseSensitive, setCaseSensitive] = useState(false);
  let [searchInput, setSearchInput] = useState("");

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
          if (
            ((!isSearchingEverywhere && e.shiftKey) || isSearchingEverywhere) &&
            e.key === "Enter"
          ) {
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
        {isSearchingEverywhere
          ? "🛰️ searching everywhere"
          : "search everywhere"}
      </button>
    </div>
  );

  let filteredItems = items;
  if (searchInput !== "") {
    let flags = caseSensitive ? "" : "i";
    filteredItems = items.filter(
      (item) => item.filename.search(new RegExp(searchInput, flags)) >= 0
    );
  }

  let noResults = (
    <div className="centerme">
      no results {isSearchingEverywhere ? "anywhere 🛰️" : ""}
    </div>
  );

  let logoThing = (
    <div className="centerme">
      <img
        id="logoThing"
        src={pageRoot + "/.dop/assets/apple-touch-icon.png"}
      />
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

  let here = items.length;
  let inSubdirs = numFiles - here + 1; // why +1??
  let numDirs = items.filter((item) => item.kind === "Dir").length;

  return (
    <>
      {searchWidget}
      <div className="header row">
        <div className="header" id="topleft"></div>
        <div className="header filename">filename</div>
        <div className="header created">created</div>
        <div className="header modified">modified</div>
        <div className="header accessed">accessed</div>
      </div>
      {filteredItems.map((item) => {
        return <PageItemListRow key={item.basename} item={item} />;
      })}
      <div id="numfiles">
        {searchInput !== "" ? <>{filteredItems.length} of</> : ""}
        {here}
        {numDirs !== 0 ? <>/ {inSubdirs} </> : ""}
      </div>
      {filteredItems.length === 0 ? noResults : ""}
      {logoThing}
    </>
  );
}

let items: types.PageItem[] = JSON.parse(
  document.getElementById("items")!.innerText
);
let pathSep = document.getElementById("pathSep")!.innerText;
let fileDir = document.getElementById("fileDir")!.innerText;
let numFiles = parseInt(document.getElementById("numFiles")!.innerText);
let pageRoot = document.getElementById("pageRoot")!.innerText;

let root = document.getElementById("reactRoot")!;
createRoot(root).render(
  <PageItemList
    items={items}
    pathSep={pathSep}
    fileDir={fileDir}
    numFiles={numFiles}
    pageRoot={pageRoot}
  />
);
