struct C { char a; char b; char c; };
struct C g = { 'X', 'Y', 'Z' };
int sz(void) {
  return sizeof(g);
}
