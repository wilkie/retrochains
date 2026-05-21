int sized(int p[10]) { return sizeof(p); }
int main(void) {
  int a[10];
  return sizeof(a) + sized(a);
}
