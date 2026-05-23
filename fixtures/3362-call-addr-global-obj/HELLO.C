int g = 10;
void use(int *p);

void driver(void) {
  use(&g);
}
