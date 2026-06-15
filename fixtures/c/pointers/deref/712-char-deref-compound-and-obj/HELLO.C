char g;
int main() {
  char *p;
  p = &g;
  *p &= 15;
  return *p;
}
