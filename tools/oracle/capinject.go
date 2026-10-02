// capinject inserts zzcapture calls at the top of named functions in a Go file.
// usage: capinject <file> <dir> <fn1,fn2,...>
package main

import (
	"go/ast"
	"go/parser"
	"go/token"
	"os"
	"sort"
	"strings"
)

func main() {
	file, dir, names := os.Args[1], os.Args[2], strings.Split(os.Args[3], ",")
	want := map[string]bool{}
	for _, n := range names {
		want[n] = true
	}
	src, _ := os.ReadFile(file)
	fset := token.NewFileSet()
	f, err := parser.ParseFile(fset, file, src, 0)
	if err != nil {
		panic(err)
	}
	type ins struct {
		off  int
		text string
	}
	var inserts []ins
	for _, d := range f.Decls {
		fd, ok := d.(*ast.FuncDecl)
		if !ok || fd.Recv != nil || !want[fd.Name.Name] || fd.Body == nil {
			continue
		}
		var params []string
		var renames []ins
		for _, p := range fd.Type.Params.List {
			for _, n := range p.Names {
				name := n.Name
				if name == "_" {
					name = "__cp" + string(rune('a'+len(params)))
					renames = append(renames, ins{fset.Position(n.Pos()).Offset, name})
				}
				params = append(params, name)
			}
		}
		var call string
		switch len(params) {
		case 3: // model, body, stream
			call = "zzcapture.Req(\"" + dir + "\",\"" + fd.Name.Name + "\"," + strings.Join(params, ",") + ")"
		case 6: // ctx, model, orig, req, raw, param
			call = "zzcapture.Resp(\"" + dir + "\",\"" + fd.Name.Name + "\"," + strings.Join(params[1:], ",") + ")"
		default:
			continue
		}
		inserts = append(inserts, renames...)
		inserts = append(inserts, ins{fset.Position(fd.Body.Lbrace).Offset + 1, "\n" + call + "\n"})
	}
	if len(inserts) == 0 {
		return
	}
	sort.Slice(inserts, func(i, j int) bool { return inserts[i].off > inserts[j].off })
	out := string(src)
	for _, in := range inserts {
		if strings.HasPrefix(in.text, "__cp") {
			out = out[:in.off] + in.text + out[in.off+1:]
		} else {
			out = out[:in.off] + in.text + out[in.off:]
		}
	}
	// add import
	idx := strings.Index(out, "import (")
	imp := "\"github.com/router-for-me/CLIProxyAPI/v8/internal/zzcapture\"\n"
	if idx >= 0 {
		out = out[:idx+8] + "\n" + imp + out[idx+8:]
	} else {
		ps := strings.Index(out, "package ")
		pk := ps + strings.Index(out[ps:], "\n")
		out = out[:pk+1] + "import " + imp + out[pk+1:]
	}
	os.WriteFile(file, []byte(out), 0o644)
	println("injected", len(inserts), file)
}
