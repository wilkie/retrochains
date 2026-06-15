void early(int x) {
  if (x < 0) return;
  x = x + 1;
}
int main(void) {
  early(-5);
  early(3);
  return 0;
}
