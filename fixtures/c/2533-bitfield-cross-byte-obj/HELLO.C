struct Wide {
  unsigned a : 5;
  unsigned b : 5;
  unsigned c : 5;
};
struct Wide w;
int main(void) {
  w.b = 17;
  return w.b;
}
