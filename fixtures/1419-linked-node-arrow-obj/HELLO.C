struct N { int v; struct N *next; };
struct N b = {2, 0};
struct N a = {1, &b};
int main(void) {
  return a.next->v;
}
