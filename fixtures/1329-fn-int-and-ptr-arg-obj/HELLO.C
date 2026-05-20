int g;
void setIf(int x, int *p) {
  if (x > 0) *p = x;
}
int main(void) {
  g = 0;
  setIf(5, &g);
  return g;
}
