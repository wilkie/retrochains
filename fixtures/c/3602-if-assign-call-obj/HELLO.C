int get(void);
int g;

void try(void) {
  if ((g = get()) != 0) g++;
}
