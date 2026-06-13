struct Pkg { long v; };
struct Pkg make(void);
int main(void) {
  struct Pkg p;
  p = make();
  return (int)p.v;
}
